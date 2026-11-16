import Foundation

/// The native upload owner's bounded, content-free evidence. Progress is
/// never translated into a publication receipt by the interface.
struct FileUploadStatus: Decodable, Equatable {
    struct Selection: Decodable, Equatable {
        let token: UInt64
        let state: String
        let selectedBytes: UInt64?

        enum CodingKeys: String, CodingKey {
            case token, state
            case selectedBytes = "selected_bytes"
        }
    }

    struct Transfer: Decodable, Equatable, Identifiable {
        let batch: UInt32
        let selectedBytes: UInt64
        let phase: String
        let stage: String?
        let queuedBytes: UInt64?
        let totalBytes: UInt64?
        let cancelRequested: Bool
        let pauseRequested: Bool
        let paused: Bool?
        let receipt: String
        let receiptDetail: String?
        let absence: String?
        let publishedBytes: UInt64?
        let cleanup: String
        let complete: Bool
        let success: Bool

        var id: UInt32 { batch }

        enum CodingKeys: String, CodingKey {
            case batch, phase, stage, paused, receipt, absence, cleanup, complete, success
            case selectedBytes = "selected_bytes"
            case queuedBytes = "queued_bytes"
            case totalBytes = "total_bytes"
            case cancelRequested = "cancel_requested"
            case pauseRequested = "pause_requested"
            case receiptDetail = "receipt_detail"
            case publishedBytes = "published_bytes"
        }

        /// An unused optional lane can be refused without there ever having
        /// been an upload. These names come from the native typed Absence enum.
        var unavailableMessage: String? {
            switch absence {
            case .some("NotNegotiated"): return "This host does not offer file receiving."
            case .some("WithClipboard"): return "This connection cannot combine files with clipboard sharing."
            case .some(_): return "File transfer is unavailable for this session."
            case .none:
                return phase == "unavailable" ? "File transfer is unavailable for this session." : nil
            }
        }

        var title: String {
            if batch == 0 {
                return unavailableMessage ?? "No file sent"
            }
            switch receipt {
            case "host_published":
                return cleanup == "failed" ? "Host received the file; local cleanup failed"
                    : cleanup == "finished" ? "File received by host" : "Host received the file; finishing cleanup"
            case "host_published_durability_unknown":
                return "Host published the file; durability unconfirmed"
            case "host_refused": return "The host refused this file"
            case "interrupted_before_publication": return "Transfer stopped before publication"
            case "publication_unknown": return "Host publication is unconfirmed"
            default: break
            }
            if cancelRequested { return complete ? "Upload stopped" : "Stopping this upload…" }
            if paused == true { return "Upload paused" }
            if pauseRequested { return "Pausing this upload…" }
            switch phase {
            case "waiting_for_control": return "Waiting for control"
            case "negotiating": return "Opening the file channel…"
            case "waiting_for_selection", "no_file_selected": return "No file sent"
            case "selection_accepted", "selected": return "File selected for this upload"
            case "sending": return "Sending file…"
            case "unavailable": return unavailableMessage ?? "File transfer is unavailable"
            case "finished", "ended": return "Upload ended without a confirmed receipt"
            case "cancelled_before_start": return "Upload cancelled before sending"
            default: return "Waiting for upload status"
            }
        }

        var progress: Double? {
            guard batch > 0, let queuedBytes, let totalBytes, totalBytes > 0 else { return nil }
            return min(1, Double(queuedBytes) / Double(totalBytes))
        }

        var progressText: String? {
            guard batch > 0, let queuedBytes, let totalBytes else { return nil }
            return "\(Self.bytes(queuedBytes)) of \(Self.bytes(totalBytes)) queued"
        }

        var canCancel: Bool { batch > 0 && !complete && !cancelRequested }
        var canPause: Bool { canCancel && phase == "sending" }

        static func bytes(_ value: UInt64) -> String {
            if value > UInt64(Int64.max) { return "\(value) bytes" }
            return ByteCountFormatter.string(fromByteCount: Int64(value), countStyle: .file)
        }
    }

    let version: UInt32
    let available: Bool
    let ready: Bool
    let sourcesReleased: Bool
    let selection: Selection
    let transfer: Transfer?
    let history: [Transfer]

    enum CodingKeys: String, CodingKey {
        case version, available, ready, selection, transfer, history
        case sourcesReleased = "sources_released"
    }

    /// The C copy API is bounded independently. A malformed or newer schema
    /// cannot enable a Send button or allocate arbitrary UI.
    static func decode(_ data: Data) -> FileUploadStatus? {
        guard data.count <= 16 * 1024,
              let status = try? JSONDecoder().decode(Self.self, from: data),
              status.version == 1, status.history.count <= 9,
              ["none", "choosing", "preparing", "prepared"].contains(status.selection.state),
              (status.selection.state == "none") == (status.selection.token == 0),
              status.selection.state != "prepared" || status.selection.selectedBytes != nil,
              status.history.allSatisfy({ (1...9).contains($0.batch) }),
              Set(status.history.map(\.batch)).count == status.history.count,
              status.transfer.map({ $0.batch <= 9 }) ?? true else { return nil }
        return status
    }
}

/// Local presentation of one native selection token. This never holds a
/// path, file contents, authority, or a retryable Send action.
struct FileUploadChoice: Equatable, Identifiable {
    enum Phase: Equatable { case choosing, preparing, prepared }
    let token: UInt64
    private(set) var phase = Phase.choosing
    private(set) var name: String?
    var id: UInt64 { token }

    mutating func beginPreparation(token: UInt64, name: String) -> Bool {
        guard self.token == token, phase == .choosing else { return false }
        self.name = Self.displayName(name)
        phase = .preparing
        return true
    }

    mutating func prepared(token: UInt64) -> Bool {
        guard self.token == token, phase == .preparing else { return false }
        phase = .prepared
        return true
    }

    /// Names stay local to this one choice and are bounded before display.
    /// Control characters from a provider never create misleading extra rows.
    static func displayName(_ name: String) -> String {
        let scalars = name.unicodeScalars.prefix(256).filter {
            !CharacterSet.controlCharacters.contains($0)
                && !CharacterSet.illegalCharacters.contains($0)
        }
        let cleaned = String(String.UnicodeScalarView(scalars)).trimmingCharacters(in: .whitespacesAndNewlines)
        return cleaned.isEmpty ? "Selected file" : cleaned
    }
}
