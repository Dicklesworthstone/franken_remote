//
// SessionControllerTests.swift — Unit tests for FrankenRemoteKit Swift wrapper
//

import XCTest
@testable import FrankenRemoteKit
@testable import CFrankenRemote

final class SessionControllerTests: XCTestCase {
    @MainActor
    func testSessionInitializationAndErrorHandling() throws {
        // Empty host address should fail with invalidArgument
        XCTAssertThrowsError(try FrankenSessionController(hostAddress: "", authToken: "token")) { error in
            guard let frankenErr = error as? FrankenError else {
                XCTFail("Expected FrankenError but got \(error)")
                return
            }
            XCTAssertEqual(frankenErr, .invalidArgument)
        }
    }

    @MainActor
    func testSessionDoubleCloseSafety() throws {
        // Create session with valid params
        let controller = try FrankenSessionController(hostAddress: "100.64.0.1:443", authToken: "valid_token")
        XCTAssertEqual(controller.state, .disconnected)

        // First close
        controller.close()

        // Second close must be safe and idempotent
        controller.close()

        // Sending input after close should safely no-op without crash
        controller.sendPointer(x: 100, y: 200, action: .down, button: .primary)
        controller.sendText("test")
    }

    @MainActor
    func testGeometryAndQualityDefaults() {
        XCTAssertEqual(DisplayGeometry.zero.scale, 1.0)
        XCTAssertEqual(ConnectionQuality.initial.qualityTier, .poor)
    }
}
