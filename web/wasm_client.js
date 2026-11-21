"use strict";
// Load the self-hosted Rust protocol owner. No protocol/session implementation
// lives here: this boundary only initializes the matching generated WASM pair.
(function (global) {
    const ABI_VERSION = 1;
    const MAX_PENDING_ADAPTERS = 16;
    let readyModule = null;
    let pendingLoad = null;
    const subscribers = new Set();

    function unavailable(code) {
        const descriptions = {
            browser_https_required: "Browser sessions require same-origin HTTPS.",
            browser_connection_cancelled: "Connection attempt was cancelled.",
            browser_client_busy: "The browser client is busy. Try again shortly.",
            browser_client_version_mismatch:
                "The browser client files do not match this page. Reload after updating the host.",
            browser_client_unavailable:
                "This host has no usable browser client package. Build and install its browser assets.",
        };
        const error = new Error(descriptions[code]);
        error.code = code;
        return error;
    }

    async function loadModule() {
        let module;
        try {
            module = await import("/pkg/fr_web.js");
            await module.default({ module_or_path: "/pkg/fr_web_bg.wasm" });
        } catch (_) {
            throw unavailable("browser_client_unavailable");
        }
        if (typeof module.browserAbiVersion !== "function"
            || module.browserAbiVersion() !== ABI_VERSION
            || typeof module.BrowserAdapter !== "function"
            || typeof module.maxBootstrapBytes !== "function"
            || typeof module.maxControlRecordBytes !== "function") {
            throw unavailable("browser_client_version_mismatch");
        }
        const maxBootstrapBytes = module.maxBootstrapBytes();
        const maxControlRecordBytes = module.maxControlRecordBytes();
        if (maxBootstrapBytes !== 4096 || maxControlRecordBytes !== 4096) {
            throw unavailable("browser_client_version_mismatch");
        }
        return { module, maxBootstrapBytes, maxControlRecordBytes };
    }

    function makeAdapter(loaded) {
        return {
            core: new loaded.module.BrowserAdapter(),
            maxBootstrapBytes: loaded.maxBootstrapBytes,
            maxControlRecordBytes: loaded.maxControlRecordBytes,
        };
    }

    function removeSubscriber(subscriber) {
        if (!subscribers.delete(subscriber)) return false;
        if (subscriber.signal) {
            subscriber.signal.removeEventListener("abort", subscriber.aborted);
        }
        return true;
    }

    function finishSubscriber(subscriber, loaded, error) {
        if (!removeSubscriber(subscriber)) return;
        if (subscriber.signal && subscriber.signal.aborted) {
            subscriber.reject(unavailable("browser_connection_cancelled"));
        } else if (error) {
            subscriber.reject(error);
        } else {
            try { subscriber.resolve(makeAdapter(loaded)); }
            catch (_) { subscriber.reject(unavailable("browser_client_unavailable")); }
        }
    }

    function startLoading() {
        if (pendingLoad) return;
        const load = {};
        pendingLoad = load;
        // One completion handler owns module loading regardless of retry count.
        // Retired callers are removed from subscribers instead of remaining as
        // continuations on a native import promise that cannot be aborted.
        loadModule().then((loaded) => {
            if (pendingLoad !== load) return;
            pendingLoad = null;
            readyModule = loaded;
            for (const subscriber of Array.from(subscribers)) {
                finishSubscriber(subscriber, loaded, null);
            }
        }, (cause) => {
            if (pendingLoad !== load) return;
            // Retire shared loading before notifying callers that may retry.
            pendingLoad = null;
            const code = cause && cause.code === "browser_client_version_mismatch"
                ? "browser_client_version_mismatch" : "browser_client_unavailable";
            const error = unavailable(code);
            for (const subscriber of Array.from(subscribers)) {
                finishSubscriber(subscriber, null, error);
            }
        });
    }

    global.FrankenRust = Object.freeze({
        createAdapter(signal) {
            if (global.location.protocol !== "https:") {
                return Promise.reject(unavailable("browser_https_required"));
            }
            if (signal && signal.aborted) {
                return Promise.reject(unavailable("browser_connection_cancelled"));
            }
            if (readyModule) {
                try { return Promise.resolve(makeAdapter(readyModule)); }
                catch (_) { return Promise.reject(unavailable("browser_client_unavailable")); }
            }
            if (subscribers.size >= MAX_PENDING_ADAPTERS) {
                return Promise.reject(unavailable("browser_client_busy"));
            }
            return new Promise((resolve, reject) => {
                const subscriber = { signal, resolve, reject, aborted: null };
                subscriber.aborted = () => {
                    if (removeSubscriber(subscriber)) {
                        reject(unavailable("browser_connection_cancelled"));
                    }
                };
                subscribers.add(subscriber);
                if (signal) {
                    signal.addEventListener("abort", subscriber.aborted, { once: true });
                    if (signal.aborted) subscriber.aborted();
                }
                if (subscribers.has(subscriber)) startLoading();
            });
        },
    });
})(window);
