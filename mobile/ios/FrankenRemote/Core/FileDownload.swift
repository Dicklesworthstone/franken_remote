import Foundation

/// Content-free receive evidence from the original native owner. A completed
/// byte count, a published file and completed cleanup are different facts.
struct FileDownloadStatus: Decodable, Equatable {
    struct Progress: Decodable, Equatable {
        let id: UInt64
        let stagedBytes: UInt64
        let totalBytes: UInt64
        enum CodingKeys: String, CodingKey {
            case id
            case stagedBytes = "staged_bytes"
            case totalBytes = "total_bytes"
        }
    }

    struct Receipt: Decodable, Equatable, Identifiable {
        let id: UInt64
        let index: UInt64?
        let outcome: String
        let receipt: String
        let bytes: UInt64?
        let durable: Bool?
        let reason: String?

        var published: Bool {
            id > 0 && index == id - 1 && outcome == "locally_published"
                && ["published", "published_durability_unknown"].contains(receipt)
                && bytes != nil && durable != nil
        }

        var title: String {
            if published {
                return durable == true ? "File saved on this device" : "File saved; durability unconfirmed"
            }
            switch outcome {
            case "cancelled": return "File cancelled before publication"
            case "refused": return "File refused"
            default: return "File publication unconfirmed"
            }
        }
    }

    let version: UInt32
    let configured: Bool
    let phase: String
    let cancelRequested: Bool
    let absence: String?
    let progress: Progress?
    let receipts: [Receipt]
    let receiptsTruncated: Bool
    let cleanup: String
    let cleanupComplete: Bool
    let success: Bool

    enum CodingKeys: String, CodingKey {
        case version, configured, phase, absence, progress, receipts, cleanup, success
        case cancelRequested = "cancel_requested"
        case receiptsTruncated = "receipts_truncated"
        case cleanupComplete = "cleanup_complete"
    }

    var title: String {
        if cleanup == "failed" { return "File cleanup failed; saved receipts are retained" }
        if let absence {
            switch absence {
            case "host_download_unavailable": return "This host does not offer file sending"
            case "files_with_clipboard_unsupported": return "This session cannot combine receiving and clipboard sharing"
            case "receive_limit": return "The receive size limit was reached"
            case "receive_integrity": return "File verification failed"
            case "receive_cancelled": return "Receiving stopped"
            case "receive_conflict": return "A file could not be saved without replacing another"
            default: return "Receiving is unavailable for this session"
            }
        }
        if cancelRequested { return cleanupComplete ? "Receiving stopped" : "Stopping receiving…" }
        switch phase {
        case "selected", "waiting_for_control": return "Ready to receive when you take control"
        case "negotiating": return "Opening the receive channel…"
        case "receiving": return progress == nil ? "Waiting for the host to send a file" : "Receiving file…"
        case "ended": return cleanupComplete ? "Receiving ended" : "Finishing file cleanup…"
        case "cancelled_before_start": return "Receiving cancelled before starting"
        default: return "Waiting for receive status"
        }
    }

    var canCancel: Bool {
        !cancelRequested && absence == nil
            && ["selected", "waiting_for_control", "negotiating", "receiving"].contains(phase)
    }

    var progressFraction: Double? {
        guard let progress, progress.totalBytes > 0 else { return nil }
        return Double(progress.stagedBytes) / Double(progress.totalBytes)
    }

    static func decode(_ data: Data) -> FileDownloadStatus? {
        guard data.count <= 32 * 1024,
              let status = try? JSONDecoder().decode(Self.self, from: data),
              status.version == 1, status.receipts.count <= 64,
              Set(status.receipts.map(\.id)).count == status.receipts.count,
              ["selected", "waiting_for_control", "negotiating", "receiving", "ended",
               "cancelled_before_start", "unavailable"].contains(status.phase),
              ["not_started", "pending", "finished", "failed"].contains(status.cleanup),
              status.cleanupComplete == ["not_started", "finished"].contains(status.cleanup),
              status.progress.map({ $0.id > 0 && $0.stagedBytes <= $0.totalBytes
                  && $0.totalBytes <= FileDownloadPolicy.maximumFileBytes }) ?? true,
              status.receipts.allSatisfy({ receipt in
                  guard receipt.index == (receipt.id == 0 ? nil : receipt.id - 1) else { return false }
                  if receipt.outcome == "locally_published" {
                      return receipt.published && receipt.bytes! <= FileDownloadPolicy.maximumFileBytes
                          && (receipt.receipt == "published") == (receipt.durable == true)
                  }
                  return ["cancelled", "refused", "publication_unknown"].contains(receipt.outcome)
                      && receipt.bytes == nil && receipt.durable == nil
              }),
              !status.success || (status.configured && status.phase == "ended"
                  && status.cleanup == "finished" && status.absence == nil
                  && status.progress == nil && !status.receiptsTruncated
                  && status.receipts.allSatisfy(\.published)) else { return nil }
        return status
    }
}

/// The phone's explicit local disk budget and local export boundary. Native
/// ATP owns transfer integrity, path safety and publication; these checks do
/// not replace it or infer receipt success from a filesystem listing.
enum FileDownloadPolicy {
    static let maximumFileBytes: UInt64 = 256 * 1024 * 1024
    static let maximumSessionBytes: UInt64 = 1024 * 1024 * 1024
    static let maximumListedFiles = 256
    static let maximumListedSessions = 128

    static func canExport(name: String, regular: Bool?, directory: Bool?,
                          package: Bool?, symbolicLink: Bool?, bytes: Int?) -> Bool {
        guard !name.isEmpty, name != ".", name != "..", !name.contains("/"),
              !name.hasPrefix(".fr-part-"), !name.hasPrefix(".fr-dir-"),
              regular == true, directory == false, package == false, symbolicLink == false,
              let bytes, bytes >= 0 else { return false }
        return UInt64(bytes) <= maximumFileBytes
    }
}

/// Only local UI knows filenames. This row is never sent to native diagnostics.
struct ReceivedFile: Identifiable, Equatable, Sendable {
    struct Identity: Equatable, Sendable {
        let device: UInt64
        let inode: UInt64
        let modified: Date
        let created: Date?
    }

    let url: URL
    let bytes: Int
    let session: UUID
    let identity: Identity
    var id: URL { url }
    var name: String { FileUploadChoice.displayName(url.lastPathComponent) }
}

struct ReceivedFiles: Sendable {
    var files: [ReceivedFile] = []
    var truncated = false
    var unavailable = false
}
