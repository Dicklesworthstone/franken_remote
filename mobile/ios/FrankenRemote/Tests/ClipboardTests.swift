import XCTest
@testable import FrankenRemote

final class ClipboardTests: XCTestCase {
    func testStartingControlDoesNotClaimNativeClipboardOrPublication() {
        let status = ClipboardStatus(requested: true)
        XCTAssertTrue(status.enabled)
        XCTAssertFalse(status.started)
        XCTAssertEqual(status.received, 0)
        XCTAssertEqual(status.title(controlled: false, ended: false), "Clipboard on after control is granted")
        XCTAssertEqual(status.title(controlled: true, ended: false), "Opening text clipboard…")
    }

    func testOffAndUnknownEffectsPreserveEarlierReceiptsWithoutImplyingSharing() {
        let native = FrClipboardStatus(requested: 1, enabled: 0, started: 1, reason: 2,
                                       received_submitted: 3, received_unknown: 1, received_refused: 2)
        let status = ClipboardStatus(native)
        XCTAssertEqual(status.title(controlled: true, ended: false), "Clipboard off")
        XCTAssertEqual(status.received, 3)
        XCTAssertEqual(status.uncertain, 1)
        XCTAssertEqual(status.refused, 2)
    }

    func testUnrequestedUnavailableAndEndedSessionsNeverClaimActiveSharing() {
        XCTAssertEqual(ClipboardStatus().title(controlled: true, ended: false), "Clipboard off")
        var native = FrClipboardStatus(requested: 1, enabled: 0, started: 0, reason: 1,
                                       received_submitted: 0, received_unknown: 0, received_refused: 0)
        XCTAssertEqual(ClipboardStatus(native).title(controlled: true, ended: false), "Clipboard unavailable on this host")
        native.enabled = 1
        native.reason = 0
        native.started = 1
        XCTAssertEqual(ClipboardStatus(native).title(controlled: true, ended: true), "Clipboard off")
    }
}
