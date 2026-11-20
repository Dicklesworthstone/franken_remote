import Foundation
import XCTest
@testable import FrankenRemote

final class FileDownloadTests: XCTestCase {
    private func receipt(id: UInt64 = 1, bytes: UInt64 = 19, durable: Bool = true) -> [String: Any] {
        ["id": id, "index": id > 0 ? ((id - 1) as Any) : NSNull(),
         "outcome": "locally_published", "receipt": durable ? "published" : "published_durability_unknown",
         "bytes": bytes, "durable": durable, "reason": NSNull()]
    }

    private func snapshot(phase: String = "receiving", cleanup: String = "pending",
                          receipts: [[String: Any]] = [], progress: [String: Any]? = nil,
                          cancel: Bool = false, absence: String? = nil, truncated: Bool = false,
                          success: Bool = false) throws -> FileDownloadStatus? {
        let value: [String: Any] = [
            "version": 1, "configured": true, "phase": phase, "cleanup": cleanup,
            "cleanup_complete": ["finished", "not_started"].contains(cleanup),
            "receipts": receipts, "progress": progress.map { $0 as Any } ?? NSNull(),
            "cancel_requested": cancel, "absence": absence.map { $0 as Any } ?? NSNull(),
            "receipts_truncated": truncated, "success": success,
        ]
        return FileDownloadStatus.decode(try JSONSerialization.data(withJSONObject: value))
    }

    func testFullyStagedBytesDoNotInventPublicationOrCompletedCleanup() throws {
        let status = try XCTUnwrap(try snapshot(progress: ["id": 1, "staged_bytes": 19, "total_bytes": 19]))
        XCTAssertEqual(status.progressFraction, 1)
        XCTAssertEqual(status.title, "Receiving file…")
        XCTAssertTrue(status.receipts.isEmpty)
        XCTAssertFalse(status.cleanupComplete)
        XCTAssertFalse(status.success)
        XCTAssertTrue(status.canCancel)
        XCTAssertNil(try snapshot(progress: ["id": 1, "staged_bytes": 20, "total_bytes": 19]))
        XCTAssertNil(try snapshot(progress: ["id": 0, "staged_bytes": 0, "total_bytes": 19]))
    }

    func testPublicationSurvivesCancellationAndFailedCleanupWithoutClaimingDurability() throws {
        let result = try XCTUnwrap(try snapshot(cleanup: "failed", receipts: [receipt(durable: false)], cancel: true))
        XCTAssertEqual(result.receipts.first?.bytes, 19)
        XCTAssertEqual(result.receipts.first?.title, "File saved; durability unconfirmed")
        XCTAssertTrue(result.receipts.first?.published ?? false)
        XCTAssertFalse(result.cleanupComplete)
        XCTAssertFalse(result.canCancel)
        XCTAssertFalse(result.success)
        XCTAssertEqual(result.title, "File cleanup failed; saved receipts are retained")
    }

    func testTypedAbsenceDoesNotBecomePermissionToRetryTheOriginalReceiver() throws {
        let status = try XCTUnwrap(try snapshot(phase: "unavailable", cleanup: "not_started",
                                               absence: "host_download_unavailable"))
        XCTAssertEqual(status.title, "This host does not offer file sending")
        XCTAssertFalse(status.canCancel)
        XCTAssertTrue(status.cleanupComplete)
        XCTAssertFalse(status.success)
        XCTAssertTrue(status.receipts.isEmpty)
    }

    func testOriginalReceiptIdsAndBoundsRejectFabricatedOrAmbiguousResults() throws {
        XCTAssertNil(try snapshot(receipts: [receipt(), receipt()]))
        XCTAssertNil(try snapshot(receipts: [receipt(id: 0)]))
        var mismatched = receipt()
        mismatched["index"] = 2
        XCTAssertNil(try snapshot(receipts: [mismatched]))
        var invented = receipt(durable: false)
        invented["durable"] = true
        XCTAssertNil(try snapshot(receipts: [invented]))
        XCTAssertNil(try snapshot(receipts: [receipt(bytes: FileDownloadPolicy.maximumFileBytes + 1)]))
        XCTAssertNil(try snapshot(receipts: (1...65).map { receipt(id: UInt64($0)) }))
        XCTAssertNotNil(try snapshot(receipts: (1...64).map { receipt(id: UInt64($0)) }, truncated: true))
    }

    func testOnlyOriginalCompleteEvidenceCanBeCalledSuccess() throws {
        XCTAssertNotNil(try snapshot(phase: "ended", cleanup: "finished", receipts: [receipt()], success: true))
        XCTAssertNil(try snapshot(phase: "receiving", cleanup: "finished", receipts: [receipt()], success: true))
        XCTAssertNil(try snapshot(phase: "ended", cleanup: "pending", receipts: [receipt()], success: true))
        XCTAssertNil(try snapshot(phase: "ended", cleanup: "finished", receipts: [receipt()], truncated: true, success: true))
        XCTAssertNil(try snapshot(phase: "ended", cleanup: "finished", receipts: [receipt()],
                                  absence: "receive_cancelled", success: true))
        XCTAssertNil(FileDownloadStatus.decode(Data(repeating: 32, count: 32 * 1024 + 1)))
        XCTAssertNil(FileDownloadStatus.decode(Data(#"{"version":2}"#.utf8)))
        XCTAssertNil(try snapshot(phase: "future_phase"))
        XCTAssertNil(try snapshot(cleanup: "future_cleanup"))
    }

    func testExportAllowsOnlyBoundedRegularFilesAndNeverPrivateStaging() {
        func permits(_ name: String = "report.txt", regular: Bool? = true, directory: Bool? = false,
                     package: Bool? = false, link: Bool? = false, bytes: Int? = 4) -> Bool {
            FileDownloadPolicy.canExport(name: name, regular: regular, directory: directory,
                                         package: package, symbolicLink: link, bytes: bytes)
        }
        XCTAssertTrue(permits())
        XCTAssertTrue(permits(".env"))
        XCTAssertTrue(permits(bytes: 0))
        XCTAssertTrue(permits(bytes: Int(FileDownloadPolicy.maximumFileBytes)))
        for name in ["", ".", "..", "../report", ".fr-part-original", ".fr-dir-original"] {
            XCTAssertFalse(permits(name))
        }
        XCTAssertFalse(permits(regular: nil))
        XCTAssertFalse(permits(directory: nil))
        XCTAssertFalse(permits(package: true))
        XCTAssertFalse(permits(link: true))
        XCTAssertFalse(permits(link: nil))
        XCTAssertFalse(permits(bytes: nil))
        XCTAssertFalse(permits(bytes: -1))
        XCTAssertFalse(permits(bytes: Int(FileDownloadPolicy.maximumFileBytes + 1)))
    }

    func testExportRechecksActualPathSizeAndSymlinkAfterListing() throws {
        let documents = try XCTUnwrap(FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first)
            .resolvingSymlinksInPath().standardizedFileURL
        let root = documents.appendingPathComponent("Received", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true,
                                               attributes: [.posixPermissions: 0o700])
        let id = UUID()
        let directory = root.appendingPathComponent(id.uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false,
                                               attributes: [.posixPermissions: 0o700])
        let original = directory.appendingPathComponent("chosen.txt")
        try Data([1, 2, 3]).write(to: original)
        let row = try XCTUnwrap(FileDownloadResource.checkedFile(original, session: id))
        XCTAssertEqual(FileDownloadResource.exportURL(row), original)
        XCTAssertNil(FileDownloadResource.exportURL(ReceivedFile(url: original, bytes: 4, session: id, identity: row.identity)))
        XCTAssertNil(FileDownloadResource.exportURL(ReceivedFile(url: original, bytes: 3, session: UUID(), identity: row.identity)))
        // Atomic same-length replacement used to pass URL/size-only equality.
        try Data([4, 5, 6]).write(to: original, options: [.atomic])
        let replacement = try XCTUnwrap(FileDownloadResource.checkedFile(original, session: id))
        XCTAssertEqual(replacement.bytes, row.bytes)
        XCTAssertNotEqual(replacement.identity, row.identity)
        XCTAssertNil(FileDownloadResource.exportURL(row))
        XCTAssertEqual(FileDownloadResource.exportURL(replacement), original)
        let moved = directory.appendingPathComponent("moved.txt")
        try FileManager.default.moveItem(at: original, to: moved)
        try FileManager.default.createSymbolicLink(at: original, withDestinationURL: moved)
        XCTAssertNil(FileDownloadResource.exportURL(replacement))
        let staging = directory.appendingPathComponent(".fr-part-private")
        try Data([1, 2, 3]).write(to: staging)
        XCTAssertNil(FileDownloadResource.exportURL(ReceivedFile(url: staging, bytes: 3, session: id, identity: row.identity)))
    }
}
