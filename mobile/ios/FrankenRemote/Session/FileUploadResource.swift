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

    /// The explicitly disclosed local copy runs on the one preparation worker.
    /// A normal coordinated read protects preflight while forUploading makes
    /// the OS-owned snapshot. Only that snapshot's descriptor escapes its
    /// accessor, as Apple's upload contract permits. No coordinator remains
    /// active while the user waits for control or the native upload runs.
    static func prepare(handle: FrHandle, token: UInt64, url: URL) -> Prepared {
        guard url.isFileURL else { return Prepared(status: FR_INVALID, resource: nil) }
        let resource = FileUploadResource(url)
        let coordinator = NSFileCoordinator()
        var error: NSError?
        var snapshotError: NSError?
        var status: Int32 = FR_INVALID
        coordinator.coordinate(readingItemAt: url, options: [.withoutChanges], error: &error) { selected in
            guard selected.isFileURL,
                  FileUploadCopyPolicy.preservesName(selected.lastPathComponent, original: url.lastPathComponent),
                  let original = metadata(selected), FileUploadCopyPolicy.permits(original) else { return }
            // Keep the same read claim through preflight and snapshot creation:
            // a cooperating writer cannot replace a regular file with a package
            // and have forUploading silently turn that directory into a ZIP.
            coordinator.coordinate(readingItemAt: selected, options: [.withoutChanges, .forUploading],
                                   error: &snapshotError) { snapshot in
                guard snapshot.isFileURL, let copied = metadata(snapshot),
                      FileUploadCopyPolicy.permitsSnapshot(copied, name: snapshot.lastPathComponent,
                                                           original: original, originalName: url.lastPathComponent) else { return }
                status = snapshot.withUnsafeFileSystemRepresentation { path in
                    guard let path else { return FR_INVALID }
                    return fr_session_prepare_upload(handle, token, path)
                }
            }
        }
        // The OS unlinks its snapshot on accessor return. Native preparation
        // pins the descriptor without reading it; native content reads still
        // require the later explicit Send in the current controlled view.
        // Never expose provider errors or paths in session diagnostics.
        guard error == nil, snapshotError == nil, status == FR_OK else {
            _ = fr_session_discard_upload_selection(handle, token)
            return Prepared(status: error == nil && snapshotError == nil ? status : FR_INVALID, resource: nil)
        }
        return Prepared(status: FR_OK, resource: resource)
    }

    private static func metadata(_ url: URL) -> FileUploadCopyPolicy.Metadata? {
        var current = url
        current.removeAllCachedResourceValues()
        guard let values = try? current.resourceValues(forKeys: [
            .isRegularFileKey, .isDirectoryKey, .isPackageKey, .isSymbolicLinkKey, .fileSizeKey,
        ]) else { return nil }
        return FileUploadCopyPolicy.Metadata(regular: values.isRegularFile,
                                             directory: values.isDirectory,
                                             package: values.isPackage,
                                             symbolicLink: values.isSymbolicLink,
                                             bytes: values.fileSize)
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
