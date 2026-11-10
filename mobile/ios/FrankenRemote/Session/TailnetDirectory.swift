import Foundation

/// The tailnet directory a FrankenRemote host serves (`frd run --directory`),
/// fetched through the Rust core: the same named, certificate-checked,
/// pinned connection a session uses, one request, then closed. The host
/// lists itself and this user's own untagged machines from its Tailscale.
struct TailnetDirectory: HostDirectory {
    func fetch(from host: SavedHost) async -> DirectoryResult {
        await withCheckedContinuation { (continuation: CheckedContinuation<DirectoryResult, Never>) in
            let pending = Pending(via: host.name, continuation: continuation)
            let context = Unmanaged.passRetained(pending).toOpaque()
            let status: Int32 = host.name.withCString { name in
                func start(_ pin: UnsafePointer<CChar>?) -> Int32 {
                    var config = FrConfig(host: name, port: host.port, control: false,
                                          direct: host.dialsDirectly, pin: pin)
                    return fr_directory_fetch(&config, context, { context, json in
                        guard let context else { return }
                        // Balances passRetained: the core calls back exactly once.
                        let pending = Unmanaged<Pending>.fromOpaque(context).takeRetainedValue()
                        pending.finish(String(cString: json))
                    })
                }
                if let pin = host.pin {
                    return pin.withCString { start($0) }
                }
                return start(nil)
            }
            if status != FR_OK {
                // Never called back: release here.
                let pending = Unmanaged<Pending>.fromOpaque(context).takeRetainedValue()
                pending.resume(.failed(via: host.name, end: UInt32(bitPattern: FR_END_INTERNAL),
                                       detail: status == FR_BUSY ? "Busy" : "InvalidConfiguration"))
            }
        }
    }

    /// One fetch's continuation, resumed exactly once.
    private final class Pending {
        let via: String
        private let continuation: CheckedContinuation<DirectoryResult, Never>

        init(via: String, continuation: CheckedContinuation<DirectoryResult, Never>) {
            self.via = via
            self.continuation = continuation
        }

        func finish(_ json: String) {
            resume(DirectoryResult.parse(json)
                   ?? .failed(via: via, end: UInt32(bitPattern: FR_END_INTERNAL), detail: "MalformedResult"))
        }

        func resume(_ result: DirectoryResult) {
            continuation.resume(returning: result)
        }
    }
}
