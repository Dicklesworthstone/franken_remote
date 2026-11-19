import Foundation
import UIKit
import UniformTypeIdentifiers
import XCTest
@testable import FrankenRemote

final class FileUploadTests: XCTestCase {
    private func transfer(batch: Int = 1, receipt: String = "none", complete: Bool = false,
                          cleanup: String = "pending", queued: UInt64 = 4,
                          absence: String? = nil) -> [String: Any] {
        [
            "batch": batch, "selected_bytes": 4, "phase": complete ? "finished" : "sending",
            "stage": "sending", "queued_bytes": queued, "total_bytes": 4,
            "cancel_requested": false, "pause_requested": false, "paused": false,
            "receipt": receipt, "receipt_detail": NSNull(),
            "absence": absence.map { $0 as Any } ?? NSNull(),
            "published_bytes": receipt == "host_published" ? (4 as Any) : NSNull(),
            "cleanup": cleanup, "complete": complete, "success": false,
        ]
    }

    private func snapshot(selection: [String: Any] = ["token": 0, "state": "none", "selected_bytes": NSNull()],
                          transfer: [String: Any]? = nil,
                          history: [[String: Any]] = []) throws -> FileUploadStatus? {
        let value: [String: Any] = [
            "version": 1, "available": true, "ready": false, "sources_released": false,
            "selection": selection, "transfer": transfer.map { $0 as Any } ?? NSNull(), "history": history,
        ]
        return FileUploadStatus.decode(try JSONSerialization.data(withJSONObject: value))
    }

    func testAllQueuedBytesAreNotAHostReceiptOrCompletedCleanup() throws {
        let status = try XCTUnwrap(try snapshot(transfer: transfer()))
        XCTAssertEqual(status.transfer?.progress, 1)
        XCTAssertEqual(status.transfer?.title, "Sending file…")
        XCTAssertEqual(status.transfer?.receipt, "none")
        XCTAssertEqual(status.transfer?.cleanup, "pending")
        XCTAssertFalse(status.ready)
        XCTAssertFalse(status.sourcesReleased)

        let ended = try XCTUnwrap(try snapshot(transfer: transfer(complete: true)))
        XCTAssertEqual(ended.transfer?.title, "Upload ended without a confirmed receipt")
        XCTAssertFalse(ended.transfer?.success ?? true)
    }

    func testAnUnusedEndedLaneDoesNotInventAnUploadOrMissingReceipt() throws {
        let status = try XCTUnwrap(try snapshot(transfer: transfer(batch: 0, complete: true)))
        XCTAssertEqual(status.transfer?.title, "No file sent")
        XCTAssertNil(status.transfer?.progress)
        XCTAssertNil(status.transfer?.progressText)
        XCTAssertFalse(status.transfer?.canCancel ?? true)
        XCTAssertFalse(status.transfer?.canPause ?? true)
    }

    func testADeclinedOptionalLaneExplainsWhyNoFileCanBeSent() throws {
        let status = try XCTUnwrap(try snapshot(
            selection: ["token": 7, "state": "prepared", "selected_bytes": 4],
            transfer: transfer(batch: 0, complete: true, absence: "NotNegotiated")))
        XCTAssertEqual(status.transfer?.title, "This host does not offer file receiving.")
        XCTAssertEqual(status.transfer?.unavailableMessage, "This host does not offer file receiving.")
        XCTAssertEqual(status.selection.state, "prepared", "the user can explicitly discard the local choice")
        XCTAssertEqual(status.transfer?.receipt, "none")
        XCTAssertNil(status.transfer?.progress)
    }

    func testPublicationAndCleanupOutcomesRemainIndependent() throws {
        let published = try XCTUnwrap(try snapshot(
            transfer: transfer(receipt: "host_published", complete: true, cleanup: "failed")))
        XCTAssertEqual(published.transfer?.title, "Host received the file; local cleanup failed")
        XCTAssertEqual(published.transfer?.publishedBytes, 4)
        XCTAssertFalse(published.transfer?.success ?? true)

        let uncertain = try XCTUnwrap(try snapshot(
            transfer: transfer(receipt: "host_published_durability_unknown", complete: true, cleanup: "finished")))
        XCTAssertEqual(uncertain.transfer?.title, "Host published the file; durability unconfirmed")
        XCTAssertFalse(uncertain.transfer?.success ?? true)
        XCTAssertNotEqual(uncertain.transfer?.title, "File received by host")

        let unknown = try XCTUnwrap(try snapshot(
            transfer: transfer(receipt: "publication_unknown", complete: true, cleanup: "finished")))
        XCTAssertEqual(unknown.transfer?.title, "Host publication is unconfirmed")
    }

    func testReceiptSurvivesCancelAndFailedCleanup() throws {
        var value = transfer(receipt: "host_published", complete: true, cleanup: "failed")
        value["cancel_requested"] = true
        let status = try XCTUnwrap(try snapshot(transfer: value))
        XCTAssertEqual(status.transfer?.title, "Host received the file; local cleanup failed")
        XCTAssertFalse(status.transfer?.canCancel ?? true)
        XCTAssertFalse(status.transfer?.canPause ?? true)
    }

    func testSnapshotsKeepExactBatchAndBoundHistory() throws {
        let history = (1...8).map { transfer(batch: $0, receipt: "host_refused", complete: true) }
        let status = try XCTUnwrap(try snapshot(transfer: transfer(batch: 9), history: history))
        XCTAssertEqual(status.transfer?.batch, 9)
        XCTAssertEqual(status.history.map(\.batch), Array(1...8).map { UInt32($0) })
        XCTAssertNil(try snapshot(history: Array(repeating: transfer(), count: 10)))
        XCTAssertNil(try snapshot(history: [transfer(), transfer()]))
        XCTAssertNil(try snapshot(history: [transfer(batch: 0)]))
        XCTAssertNil(try snapshot(transfer: transfer(batch: 10)))
    }

    func testMalformedOrOversizedSelectionCannotEnableSend() throws {
        let invalidSelections: [[String: Any]] = [
            ["token": 0, "state": "prepared", "selected_bytes": 4],
            ["token": 7, "state": "none", "selected_bytes": NSNull()],
            ["token": 7, "state": "prepared", "selected_bytes": NSNull()],
            ["token": 7, "state": "future_state", "selected_bytes": 4],
        ]
        for selection in invalidSelections {
            XCTAssertNil(try snapshot(selection: selection))
        }
        XCTAssertNil(FileUploadStatus.decode(Data(repeating: 32, count: 16 * 1024 + 1)))
        XCTAssertNil(FileUploadStatus.decode(Data(#"{"version":2}"#.utf8)))
        XCTAssertNil(FileUploadStatus.decode(Data(#"{"version":1,"available":true}"#.utf8)))
    }

    func testLocalChoiceCannotBeCompletedByAnOldOrDuplicateCallback() {
        var current = FileUploadChoice(token: 8)
        XCTAssertFalse(current.beginPreparation(token: 7, name: "wrong.txt"))
        XCTAssertEqual(current.phase, .choosing)
        XCTAssertNil(current.name)
        XCTAssertTrue(current.beginPreparation(token: 8, name: "chosen.txt"))
        XCTAssertFalse(current.beginPreparation(token: 8, name: "replacement.txt"))
        XCTAssertEqual(current.name, "chosen.txt")
        XCTAssertFalse(current.prepared(token: 7))
        XCTAssertEqual(current.phase, .preparing)
        XCTAssertTrue(current.prepared(token: 8))
        XCTAssertFalse(current.prepared(token: 8))
        XCTAssertEqual(current.phase, .prepared)

        let replacement = FileUploadChoice(token: 9)
        XCTAssertNotEqual(current.id, replacement.id)
        XCTAssertNil(replacement.name)
        XCTAssertEqual(replacement.phase, .choosing)
    }

    func testProviderNamesCannotInjectExtraStatusLinesOrUnboundedText() {
        XCTAssertEqual(FileUploadChoice.displayName("\n\t\r"), "Selected file")
        XCTAssertEqual(FileUploadChoice.displayName("report\nreceived.txt"), "reportreceived.txt")
        XCTAssertLessThanOrEqual(FileUploadChoice.displayName(String(repeating: "é", count: 10_000)).unicodeScalars.count, 256)
        XCTAssertEqual(FileUploadChoice.displayName("Résumé final.txt"), "Résumé final.txt")
    }

    private func copyMetadata(regular: Bool? = true, directory: Bool? = false,
                              package: Bool? = false, symbolicLink: Bool? = false,
                              bytes: Int? = 4) -> FileUploadCopyPolicy.Metadata {
        FileUploadCopyPolicy.Metadata(regular: regular, directory: directory,
                                      package: package, symbolicLink: symbolicLink, bytes: bytes)
    }

    func testUploadCopyRefusesFoldersPackagesLinksAndUnknownFileTypesBeforeSnapshot() {
        let refused = [
            copyMetadata(regular: false, directory: true),
            copyMetadata(regular: false, directory: true, package: true),
            copyMetadata(package: true),
            copyMetadata(symbolicLink: true),
            copyMetadata(regular: nil),
            copyMetadata(directory: nil),
            copyMetadata(package: nil),
            copyMetadata(symbolicLink: nil),
        ]
        for item in refused {
            XCTAssertFalse(FileUploadCopyPolicy.permits(item))
            XCTAssertFalse(FileUploadCopyPolicy.permitsSnapshot(
                copyMetadata(), name: "document.txt", original: item, originalName: "document.txt"))
        }
    }

    func testUploadCopyBoundsKnownSizeIncludingEmptyAndExactLimit() {
        for bytes in [0, FileUploadCopyPolicy.maximumBytes] {
            XCTAssertTrue(FileUploadCopyPolicy.permits(copyMetadata(bytes: bytes)))
        }
        for bytes in [nil, -1, FileUploadCopyPolicy.maximumBytes + 1, Int.max] as [Int?] {
            let oversized = copyMetadata(bytes: bytes)
            XCTAssertFalse(FileUploadCopyPolicy.permits(oversized))
            XCTAssertFalse(FileUploadCopyPolicy.permitsSnapshot(
                oversized, name: "document.txt", original: copyMetadata(), originalName: "document.txt"))
        }
    }

    func testUploadSnapshotCannotSilentlyRenameResizeOrTransformTheChosenFile() {
        let original = copyMetadata()
        XCTAssertTrue(FileUploadCopyPolicy.permitsSnapshot(
            original, name: "Résumé.txt", original: original, originalName: "Résumé.txt"))
        XCTAssertFalse(FileUploadCopyPolicy.permitsSnapshot(
            original, name: "temporary-upload.txt", original: original, originalName: "document.txt"))
        XCTAssertFalse(FileUploadCopyPolicy.permitsSnapshot(
            original, name: "folder.zip", original: copyMetadata(regular: false, directory: true),
            originalName: "folder"))
        XCTAssertFalse(FileUploadCopyPolicy.permitsSnapshot(
            copyMetadata(bytes: 5), name: "document.txt", original: original, originalName: "document.txt"))
        XCTAssertFalse(FileUploadCopyPolicy.permitsSnapshot(
            copyMetadata(regular: false, directory: true), name: "document.txt",
            original: original, originalName: "document.txt"))
        XCTAssertFalse(FileUploadCopyPolicy.preservesName("", original: ""))
        XCTAssertFalse(FileUploadCopyPolicy.preservesName(".", original: "."))
        XCTAssertFalse(FileUploadCopyPolicy.preservesName("..", original: ".."))
        // Swift String equality accepts canonical equivalents; filenames must
        // retain the exact UTF-8 bytes that the native remote name will use.
        XCTAssertFalse(FileUploadCopyPolicy.preservesName("Re\u{301}sume\u{301}.txt", original: "Résumé.txt"))
    }

    @MainActor
    func testNativePickerConsumesExactlyOneOriginalCallback() {
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.item], asCopy: false)
        var selected: [URL?] = []
        let delegate = FileUploadPicker.Coordinator { selected.append($0) }
        let original = URL(fileURLWithPath: "/selected/document.txt")
        delegate.documentPicker(picker, didPickDocumentsAt: [original])
        delegate.documentPickerWasCancelled(picker)
        delegate.documentPicker(picker, didPickDocumentsAt: [URL(fileURLWithPath: "/later/file.txt")])
        XCTAssertEqual(selected, [original])
    }

    @MainActor
    func testNativePickerRefusesMultipleOrNonFileResultsWithoutSelectingAPrefix() {
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.item], asCopy: false)
        for urls in [[], [URL(string: "https://example.invalid/file")!],
                     [URL(fileURLWithPath: "/one"), URL(fileURLWithPath: "/two")]] {
            var selected: [URL?] = []
            let delegate = FileUploadPicker.Coordinator { selected.append($0) }
            delegate.documentPicker(picker, didPickDocumentsAt: urls)
            delegate.documentPicker(picker, didPickDocumentsAt: [URL(fileURLWithPath: "/later")])
            XCTAssertEqual(selected, [nil])
        }
    }
}
