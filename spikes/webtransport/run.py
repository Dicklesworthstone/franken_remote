#!/usr/bin/env python3
"""Phase 0 WebTransport and WSS fallback qualification harness.
Runs real Google Chrome against the Asupersync WebTransport HTTP/3 and WSS endpoints.
"""

import argparse
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time


class TestServer:
    def __init__(self, spike_bin, mode, expected_origin, timeout=25):
        self.spike_bin = spike_bin
        self.mode = mode
        self.expected_origin = expected_origin
        self.timeout = timeout
        self.proc = None
        self.ready_data = None
        self.report = None

    def start(self):
        cmd = [self.spike_bin]
        if self.mode == "wt":
            cmd.extend(["serve-wt", self.expected_origin, str(self.timeout)])
        elif self.mode == "wss":
            cmd.extend(["serve-wss", str(self.timeout)])

        self.proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )

        prefix = "WT_READY:" if self.mode == "wt" else "WSS_READY:"
        for line in self.proc.stdout:
            line = line.strip()
            if line.startswith(prefix):
                self.ready_data = json.loads(line[len(prefix):])
                break
        if not self.ready_data:
            stderr = self.proc.stderr.read()
            raise RuntimeError(f"Server failed to become ready: {stderr}")

    def wait_report(self):
        prefix = "SESSION_REPORT:" if self.mode == "wt" else "WSS_REPORT:"
        for line in self.proc.stdout:
            line = line.strip()
            if line.startswith(prefix):
                self.report = json.loads(line[len(prefix):])
                break
        self.proc.wait(timeout=5)
        return self.report

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.proc.kill()


def run_chrome_test(chrome_path, html_dir, config, timeout=30):
    completed = threading.Event()
    result = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(5)

        def log_message(self, *_):
            pass

        def do_GET(self):
            if self.path in ("/", "/probe.html"):
                body = (html_dir / "probe.html").read_bytes()
                mime = "text/html"
            elif self.path == "/probe.js":
                body = (html_dir / "probe.js").read_bytes()
                mime = "text/javascript"
            elif self.path == "/config":
                body = json.dumps(config).encode()
                mime = "application/json"
            else:
                self.send_error(404)
                return

            self.send_response(200)
            self.send_header("Content-Type", mime)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            if self.path == "/result":
                length = int(self.headers.get("Content-Length", 0))
                body = self.rfile.read(length)
                result.append(json.loads(body))
                self.send_response(200)
                self.end_headers()
                completed.set()
            else:
                self.send_error(404)

    httpd = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    http_port = httpd.server_port
    web_origin = f"http://127.0.0.1:{http_port}"
    config["webOrigin"] = web_origin

    server_thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    server_thread.start()

    profile_dir = Path("/tmp") / f"chrome-wt-profile-{int(time.time() * 1000)}"
    profile_dir.mkdir(parents=True, exist_ok=True)

    cmd = [
        chrome_path,
        "--headless=new",
        f"--user-data-dir={profile_dir}",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        "--remote-debugging-port=0",
        "--enable-logging=stderr",
        "--v=1",
        f"--log-net-log=/tmp/chrome-netlog-{config.get('mode', 'test')}.json",
        "--net-log-capture-mode=Everything",
    ]

    if "port" in config:
        cmd.append(f"--origin-to-force-quic-on=127.0.0.1:{config['port']}")

    cmd.append(f"{web_origin}/probe.html")

    browser_proc = subprocess.Popen(
        cmd,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )

    chrome_err = ""
    try:
        if not completed.wait(timeout):
            browser_proc.terminate()
            raise TimeoutError("Browser test timed out waiting for result")
    finally:
        if browser_proc.poll() is None:
            os.killpg(browser_proc.pid, signal.SIGTERM)
            try:
                browser_proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                os.killpg(browser_proc.pid, signal.SIGKILL)
        if browser_proc.stderr:
            chrome_err = browser_proc.stderr.read()
        httpd.shutdown()
        httpd.server_close()

    if chrome_err:
        print("--- Chrome stderr ---")
        for line in chrome_err.splitlines():
            if any(k in line for k in ("quic", "webtransport", "WebTransport", "QUIC", "ERROR", "error")):
                print(line)
        print("---------------------")

    return result[0] if result else {"status": "failed", "reason": "no_result", "chrome_err": chrome_err}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--chrome", default="/usr/bin/google-chrome")
    parser.add_argument(
        "--spike-bin",
        default="/data/tmp/cargo-target-webtransport-spike/release/webtransport-spike",
    )
    parser.add_argument("--output", type=Path, default=Path("results/latest"))
    args = parser.parse_args()

    args.output.mkdir(parents=True, exist_ok=True)
    this_dir = Path(__file__).parent.resolve()

    # Get chrome version
    chrome_ver = (
        subprocess.check_output([args.chrome, "--version"]).decode().strip()
    )
    print(f"Testing against browser: {chrome_ver}")

    results = {
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "browser": chrome_ver,
        "scenarios": {},
    }

    # -------------------------------------------------------------
    # Scenario 1: WebTransport Happy Path
    # -------------------------------------------------------------
    print("\n=== Running Scenario 1: WebTransport Happy Path ===")
    wt_server = TestServer(args.spike_bin, "wt", "*", timeout=25)
    try:
        wt_server.start()
        port = wt_server.ready_data["port"]
        cert_hash_hex = wt_server.ready_data["cert_hash_hex"]
        cert_hash_bytes = wt_server.ready_data["cert_hash_bytes"]

        cfg = {
            "mode": "wt_happy",
            "port": port,
            "certHashHex": cert_hash_hex,
            "certHashBytes": cert_hash_bytes,
        }

        browser_res = run_chrome_test(args.chrome, this_dir, cfg, timeout=25)
        report = wt_server.wait_report()

        print(f"Browser Result: {browser_res}")
        print(f"Server Report:  {report}")

        passed = (
            browser_res.get("status") == "passed"
            and report.get("origin_accepted") is True
            and report.get("status_code") == 200
            and report.get("datagrams_received", 0) >= 5
        )

        results["scenarios"]["webtransport_happy_path"] = {
            "status": "passed" if passed else "failed",
            "browser_result": browser_res,
            "server_report": report,
        }
    finally:
        wt_server.stop()

    # -------------------------------------------------------------
    # Scenario 2: WebTransport Origin Check Negative Test
    # -------------------------------------------------------------
    print("\n=== Running Scenario 2: WebTransport Origin Rejection ===")
    wt_neg_server = TestServer(
        args.spike_bin, "wt", "https://strictly-allowed.corp", timeout=15
    )
    try:
        wt_neg_server.start()
        port = wt_neg_server.ready_data["port"]
        cert_hash_hex = wt_neg_server.ready_data["cert_hash_hex"]
        cert_hash_bytes = wt_neg_server.ready_data["cert_hash_bytes"]

        cfg = {
            "mode": "wt_origin_reject",
            "port": port,
            "certHashHex": cert_hash_hex,
            "certHashBytes": cert_hash_bytes,
        }

        browser_res = run_chrome_test(args.chrome, this_dir, cfg, timeout=15)
        report = wt_neg_server.wait_report()

        print(f"Browser Result: {browser_res}")
        print(f"Server Report:  {report}")

        passed = (
            browser_res.get("status") == "passed"
            and browser_res.get("origin_rejected") is True
            and report.get("origin_accepted") is False
            and report.get("status_code") == 403
        )

        results["scenarios"]["webtransport_origin_rejection"] = {
            "status": "passed" if passed else "failed",
            "browser_result": browser_res,
            "server_report": report,
        }
    finally:
        wt_neg_server.stop()

    # -------------------------------------------------------------
    # Scenario 3: WSS Fallback Bounded Credit & Generation Fencing
    # -------------------------------------------------------------
    print("\n=== Running Scenario 3: WSS Fallback Credit & Generation Fencing ===")
    wss_server = TestServer(args.spike_bin, "wss", "*", timeout=15)
    try:
        wss_server.start()
        wss_port = wss_server.ready_data["port"]

        cfg = {
            "mode": "wss_fallback",
            "wssPort": wss_port,
        }

        browser_res = run_chrome_test(args.chrome, this_dir, cfg, timeout=15)
        report = wss_server.wait_report()

        print(f"Browser Result: {browser_res}")
        print(f"Server Report:  {report}")

        passed = (
            browser_res.get("status") == "passed"
            and browser_res.get("credit_backpressure_verified") is True
            and browser_res.get("stale_generation_fenced") is True
            and report.get("stale_generation_rejected") is True
            and report.get("bytes_sent_before_stall") == 4096
            and report.get("bytes_sent_after_topup") == 2048
        )

        results["scenarios"]["wss_fallback"] = {
            "status": "passed" if passed else "failed",
            "browser_result": browser_res,
            "server_report": report,
        }
    finally:
        wss_server.stop()

    # Calculate overall verdict
    all_passed = all(
        s.get("status") == "passed" for s in results["scenarios"].values()
    )
    results["verdict"] = (
        "GO for browser WebTransport and WSS fallback qualification"
        if all_passed
        else "NO-GO"
    )

    out_file = args.output / "result.json"
    out_file.write_text(json.dumps(results, indent=2))
    print(f"\nSaved qualification results to {out_file}")
    print(json.dumps(results, indent=2))

    if not all_passed:
        sys.exit(1)


if __name__ == "__main__":
    main()
