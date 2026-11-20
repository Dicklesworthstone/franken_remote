import Foundation

/// The app's one-use destination and its native preparation token. The token
/// owns only a pinned directory; session start consumes it on the main actor
/// after checking that the original foreground choice is still current.
/// This immutable object is held until the original receive worker is gone.
final class FileDownloadResource: @unchecked Sendable {
    let token: UInt64
    let directory: URL
    private let session: UUID
    private static let preparation = NSLock()
    private static let activeLock = NSLock()
    private static var active: Set<UUID> = []

    enum PreparationError: Error { case busy, unavailable }

    private init(token: UInt64, directory: URL, session: UUID) {
        self.token = token
        self.directory = directory
        self.session = session
        Self.activeLock.lock()
        Self.active.insert(session)
        Self.activeLock.unlock()
    }

    deinit {
        // A consumed token is already stale; it can never cancel its session.
        _ = fr_download_destination_discard(token)
        Self.activeLock.lock()
        Self.active.remove(session)
        Self.activeLock.unlock()
    }

    /// One bounded background preparation, including directory creation.
    /// Failure and cancellation retain any directory already created; no
    /// existing local file is removed or replaced. Paths/errors are not logged.
    static func prepare() throws -> FileDownloadResource {
        guard preparation.try() else { throw PreparationError.busy }
        defer { preparation.unlock() }
        let manager = FileManager.default
        _ = try manager.url(for: .documentDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
        let root = try rootDirectory()
        if !manager.fileExists(atPath: root.path) {
            try manager.createDirectory(at: root, withIntermediateDirectories: false,
                                        attributes: [.posixPermissions: 0o700])
        }
        guard safeDirectory(root) else { throw PreparationError.unavailable }
        let session = UUID()
        let directory = root.appendingPathComponent(session.uuidString, isDirectory: true)
        try manager.createDirectory(at: directory, withIntermediateDirectories: false,
                                    attributes: [.posixPermissions: 0o700,
                                                 .protectionKey: FileProtectionType.complete])
        guard safeDirectory(directory) else { throw PreparationError.unavailable }
        var token: UInt64 = 0
        let status = directory.withUnsafeFileSystemRepresentation { path in
            fr_download_destination_prepare(path, FileDownloadPolicy.maximumFileBytes,
                                            FileDownloadPolicy.maximumSessionBytes, &token)
        }
        guard status == FR_OK, token != 0 else {
            if token != 0 { _ = fr_download_destination_discard(token) }
            throw status == FR_BUSY ? PreparationError.busy : PreparationError.unavailable
        }
        return FileDownloadResource(token: token, directory: directory, session: session)
    }

    private static func rootDirectory() throws -> URL {
        guard let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first
        else { throw PreparationError.unavailable }
        // The system's app-container path may contain iOS's /var alias. Resolve
        // that trusted path once; the actual receive root still rejects links.
        let canonical = documents.resolvingSymlinksInPath().standardizedFileURL
        guard safeDirectory(canonical) else { throw PreparationError.unavailable }
        return canonical.appendingPathComponent("Received", isDirectory: true)
    }

    private static func safeDirectory(_ url: URL) -> Bool {
        var current = url
        current.removeAllCachedResourceValues()
        guard current.isFileURL,
              let values = try? current.resourceValues(forKeys: [.isDirectoryKey, .isSymbolicLinkKey]),
              values.isDirectory == true, values.isSymbolicLink == false else { return false }
        return current.standardizedFileURL.path == current.resolvingSymlinksInPath().standardizedFileURL.path
    }

    private static func isActive(_ session: UUID) -> Bool {
        activeLock.lock()
        defer { activeLock.unlock() }
        return active.contains(session)
    }

    /// Call only after the native original-session cleanup witness. Making
    /// files visible grants no new file-receive consent or network authority.
    func allowExportAfterCleanup() {
        Self.activeLock.lock()
        Self.active.remove(session)
        Self.activeLock.unlock()
    }

    /// A bounded local listing, never a new remote request. Active or
    /// uncollected destinations remain private; only actual regular published
    /// names appear. After app/process restart no old native writer survives,
    /// so completed names from an interrupted session can be exported too.
    static func catalog() -> ReceivedFiles {
        var result = ReceivedFiles()
        guard let root = try? rootDirectory() else { result.unavailable = true; return result }
        guard FileManager.default.fileExists(atPath: root.path) else { return result }
        guard safeDirectory(root), let sessions = FileManager.default.enumerator(
            at: root, includingPropertiesForKeys: [.isDirectoryKey, .isSymbolicLinkKey],
            options: [.skipsSubdirectoryDescendants]) else {
            result.unavailable = true
            return result
        }
        var inspected = 0
        while let directory = sessions.nextObject() as? URL {
            inspected += 1
            guard inspected <= FileDownloadPolicy.maximumListedSessions else {
                result.truncated = true
                break
            }
            guard let session = UUID(uuidString: directory.lastPathComponent),
                  !isActive(session), safeDirectory(directory),
                  let files = FileManager.default.enumerator(at: directory,
                      includingPropertiesForKeys: fileKeys, options: [.skipsSubdirectoryDescendants]) else { continue }
            var entries = 0
            while let url = files.nextObject() as? URL {
                entries += 1
                guard entries <= 128, result.files.count < FileDownloadPolicy.maximumListedFiles else {
                    result.truncated = true
                    break
                }
                if let file = checkedFile(url, session: session) { result.files.append(file) }
            }
            if result.files.count >= FileDownloadPolicy.maximumListedFiles {
                result.truncated = true
                break
            }
        }
        result.files.sort { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
        return result
    }

    private static let fileKeys: [URLResourceKey] = [
        .isRegularFileKey, .isDirectoryKey, .isPackageKey, .isSymbolicLinkKey, .fileSizeKey,
    ]

    static func checkedFile(_ url: URL, session: UUID) -> ReceivedFile? {
        var current = url
        current.removeAllCachedResourceValues()
        guard current.isFileURL,
              current.standardizedFileURL.path == current.resolvingSymlinksInPath().standardizedFileURL.path,
              let values = try? current.resourceValues(forKeys: Set(fileKeys)),
              FileDownloadPolicy.canExport(name: current.lastPathComponent, regular: values.isRegularFile,
                  directory: values.isDirectory, package: values.isPackage, symbolicLink: values.isSymbolicLink,
                  bytes: values.fileSize), let bytes = values.fileSize,
              let attributes = try? FileManager.default.attributesOfItem(atPath: current.path),
              attributes[.type] as? FileAttributeType == .typeRegular,
              let actualBytes = attributes[.size] as? NSNumber, actualBytes.uint64Value == UInt64(bytes),
              let device = attributes[.systemNumber] as? NSNumber,
              let inode = attributes[.systemFileNumber] as? NSNumber,
              let modified = attributes[.modificationDate] as? Date else { return nil }
        let identity = ReceivedFile.Identity(device: device.uint64Value, inode: inode.uint64Value,
                                             modified: modified, created: attributes[.creationDate] as? Date)
        return ReceivedFile(url: current, bytes: bytes, session: session, identity: identity)
    }

    /// Recheck the row immediately before opening the system share sheet.
    /// A stale row, replaced path, symlink, changed size, package, staging file
    /// or still-active destination never escapes through a cached listing.
    /// This establishes identity at handoff; the system share sheet owns later
    /// URL access. It is not an immutable snapshot or a post-handoff guarantee.
    static func exportURL(_ file: ReceivedFile) -> URL? {
        guard !isActive(file.session), let root = try? rootDirectory(), safeDirectory(root) else { return nil }
        let directory = root.appendingPathComponent(file.session.uuidString, isDirectory: true)
        guard safeDirectory(directory),
              file.url.deletingLastPathComponent().standardizedFileURL == directory.standardizedFileURL,
              let current = checkedFile(file.url, session: file.session), current == file else { return nil }
        return current.url
    }
}
