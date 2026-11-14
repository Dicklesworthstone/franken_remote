import XCTest
@testable import FrankenRemote

final class DisplaySelectionTests: XCTestCase {
    private func info(_ index: UInt32) -> FrDisplayInfo {
        FrDisplayInfo(pixel_width: 2560, pixel_height: 1440,
                      logical_width: 1280, logical_height: 720,
                      x: -2560 * Int32(index), y: -120,
                      scale_numerator: 2, scale_denominator: 1,
                      rotation: index % 4, reserved: 0)
    }

    func testTheCompleteApprovedCatalogPreservesRowsAndOriginalToken() throws {
        let request = FrDisplayRequest(token: 91, count: UInt32(FR_MAX_DISPLAYS), reserved: 0)
        var copied: [(UInt64, UInt32)] = []
        let selection = try XCTUnwrap(DisplaySelection.copy(request) { token, index in
            copied.append((token, index))
            return self.info(index)
        })
        XCTAssertEqual(selection.token, 91)
        XCTAssertEqual(selection.displays.count, Int(FR_MAX_DISPLAYS))
        XCTAssertEqual(copied.map(\.0), Array(repeating: 91, count: Int(FR_MAX_DISPLAYS)))
        XCTAssertEqual(copied.map(\.1), Array(0..<UInt32(FR_MAX_DISPLAYS)))
        XCTAssertEqual(selection.displays.last?.id, 7)
        XCTAssertEqual(selection.displays.last?.x, -17920)
        XCTAssertEqual(selection.displays.last?.y, -120)
        XCTAssertEqual(selection.displays.last?.logicalWidth, 1280)
        XCTAssertEqual(selection.displays.last?.rotation, 3)
        XCTAssertEqual(selection.displays.last?.title, "Display 8")
        XCTAssertEqual(selection.displays.last?.pixels, "2560 × 1440 pixels")
    }

    func testPartialAndUnboundedSnapshotsNeverBecomeSelectableLists() {
        var copied: [UInt32] = []
        let request = FrDisplayRequest(token: 91, count: 3, reserved: 0)
        XCTAssertNil(DisplaySelection.copy(request) { _, index in
            copied.append(index)
            return index == 1 ? nil : self.info(index)
        })
        XCTAssertEqual(copied, [0, 1])

        for request in [FrDisplayRequest(token: 0, count: 1, reserved: 0),
                        FrDisplayRequest(token: 91, count: 0, reserved: 0),
                        FrDisplayRequest(token: 91, count: UInt32(FR_MAX_DISPLAYS) + 1, reserved: 0),
                        FrDisplayRequest(token: 91, count: 1, reserved: 1)] {
            XCTAssertNil(DisplaySelection.copy(request) { _, _ in
                XCTFail("invalid bounds must be refused before copying any row")
                return self.info(0)
            })
        }
    }

    func testInvalidBridgeMetadataRefusesTheWholeSnapshot() {
        let request = FrDisplayRequest(token: 91, count: 2, reserved: 0)
        var invalid = info(1)
        invalid.scale_denominator = 0
        XCTAssertNil(DisplaySelection.copy(request) { _, index in index == 0 ? self.info(0) : invalid })
        invalid = info(1)
        invalid.rotation = 4
        XCTAssertNil(DisplaySelection.copy(request) { _, index in index == 0 ? self.info(0) : invalid })
        invalid = info(1)
        invalid.pixel_width = 0
        XCTAssertNil(DisplaySelection.copy(request) { _, index in index == 0 ? self.info(0) : invalid })
    }

    func testReopenRequiresActualCleanupAndTheExistingHostCooldown() {
        var reopen = DisplayReopen()
        XCTAssertEqual(reopen.poll(status: FR_PENDING, elapsed: .seconds(1)), .waiting)
        XCTAssertEqual(reopen.poll(status: FR_OK, elapsed: .seconds(2)), .waiting)
        XCTAssertEqual(reopen.poll(status: FR_OK, elapsed: .milliseconds(2399)), .waiting)
        XCTAssertEqual(reopen.poll(status: FR_OK, elapsed: .milliseconds(2400)), .ready)
        XCTAssertEqual(reopen.poll(status: FR_OK, elapsed: .seconds(3)), .refused,
                       "the same local switch can never open a second session")
    }

    func testCleanupRefusalExpiryAndLostOwnerNeverTurnIntoAnAutomaticRetry() {
        for status in [FR_CLOSED, FR_STALE, FR_INVALID] {
            var reopen = DisplayReopen()
            XCTAssertEqual(reopen.poll(status: status, elapsed: .seconds(1)), .refused)
            XCTAssertEqual(reopen.poll(status: FR_OK, elapsed: .seconds(2)), .refused)
        }
        var timeout = DisplayReopen()
        XCTAssertEqual(timeout.poll(status: FR_PENDING, elapsed: .milliseconds(9999)), .waiting)
        XCTAssertEqual(timeout.poll(status: FR_OK, elapsed: .seconds(10)), .refused)
        XCTAssertEqual(timeout.poll(status: FR_OK, elapsed: .seconds(11)), .refused)

        var interrupted = DisplayReopen()
        XCTAssertEqual(interrupted.poll(status: FR_OK, elapsed: .seconds(1)), .waiting)
        XCTAssertEqual(interrupted.poll(status: FR_STALE, elapsed: .milliseconds(1200)), .refused)
        XCTAssertEqual(interrupted.poll(status: FR_OK, elapsed: .seconds(2)), .refused)
    }

    func testDisplaySwitchKeepsTheOriginalDestinationAndActualPin() throws {
        let original = SavedHost(id: UUID(), name: "desktop.tailtest.ts.net", port: 18443,
                                 nickname: "Office", pin: nil, direct: true)
        let pin = "fr-pin-v1 desktop.tailtest.ts.net 100.64.0.9"
        let next = try XCTUnwrap(DisplayReopen.pinnedHost(original, observedPin: pin))
        XCTAssertEqual(next.id, original.id)
        XCTAssertEqual(next.name, original.name)
        XCTAssertEqual(next.port, 18443)
        XCTAssertEqual(next.nickname, "Office")
        XCTAssertEqual(next.pin, pin)
        XCTAssertTrue(next.dialsDirectly)
        XCTAssertNil(DisplayReopen.pinnedHost(original, observedPin: nil))
        XCTAssertNil(DisplayReopen.pinnedHost(original, observedPin: ""))
        XCTAssertEqual(DisplayReopen.pinnedHost(next, observedPin: nil), next)
    }
}
