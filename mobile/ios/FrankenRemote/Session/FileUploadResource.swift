import Foundation

/// One document-provider permission. Immutable after construction, so moving
/// the owner between the preparation worker and main actor is safe. Its final
/// release balances the original successful access, after every native reader
/// has released that resource; it never stores a bookmark or file contents.
final class FileUploadResource: @unchecked Sendable {
    private let url: URL
    private let accessing: Bool

    private init(_ url: URL) {
        self.url = url
        accessing = url.startAccessingSecurityScopedResource()
    }

    deinit {
        if accessing { url.stopAccessingSecurityScopedResource() }
    }

    struct Prepared: Sendable {
        let status: Int32
        let resource: FileUploadResource?
    }

    /// The caller runs this on a bounded background task. Provider coordination
    /// and descriptor preparation never block the display or session thread.
    /// A URL already inside the sandbox may need no scope; the native open
    /// still enforces actual access and the original regular-file checks.
    static func prepare(handle: FrHandle, token: UInt64, url: URL) -> Prepared {
        guard url.isFileURL else { return Prepared(status: FR_INVALID, resource: nil) }
        let resource = FileUploadResource(url)
        let coordinator = NSFileCoordinator()
        var error: NSError?
        var status: Int32 = FR_INVALID
        coordinator.coordinate(readingItemAt: url, options: [.withoutChanges], error: &error) { selected in
            guard selected.isFileURL else { return }
            status = selected.withUnsafeFileSystemRepresentation { path in
                guard let path else { return FR_INVALID }
                return fr_session_prepare_upload(handle, token, path)
            }
        }
        // Never expose native provider errors or paths in session diagnostics.
        guard error == nil, status == FR_OK else {
            _ = fr_session_discard_upload_selection(handle, token)
            return Prepared(status: error == nil ? status : FR_INVALID, resource: nil)
        }
        return Prepared(status: FR_OK, resource: resource)
    }

    /// Free only asks the native owner to stop. A detached session can still
    /// read files after the UI disappears, including after a cleanup deadline.
    /// Hold the original registry handle and scopes until its separate positive
    /// source-release witness. MAX_SESSIONS bounds unresolved owners; failed
    /// cleanup cannot recycle their permissions.
    static func retire(handle: FrHandle, resources: [FileUploadResource]) {
        _ = fr_session_close(handle)
        if resources.isEmpty {
            _ = fr_session_free(handle)
            return
        }
        Task.detached(priority: .utility) {
            while !fr_session_upload_sources_released(handle) {
                try? await Task.sleep(for: .milliseconds(250))
            }
            withExtendedLifetime(resources) {
                _ = fr_session_free(handle)
            }
        }
    }
}
