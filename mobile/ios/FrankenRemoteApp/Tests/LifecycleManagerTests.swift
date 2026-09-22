//
// LifecycleManagerTests.swift — Unit tests for iOS lifecycle & background authority
//

import XCTest
@testable import FrankenRemoteApp

final class LifecycleManagerTests: XCTestCase {
    @MainActor
    func testBackgroundTransitionTriggersCallback() {
        let manager = AppLifecycleManager()
        var backgroundCallbackCalled = false
        var foregroundCallbackCalled = false

        manager.onBackgroundEntered = {
            backgroundCallbackCalled = true
        }
        manager.onForegroundResumed = {
            foregroundCallbackCalled = true
        }

        // Simulate entering background
        manager.handleScenePhaseChange(.background)
        XCTAssertTrue(backgroundCallbackCalled)
        XCTAssertFalse(foregroundCallbackCalled)

        // Simulate resuming to active
        manager.handleScenePhaseChange(.active)
        XCTAssertTrue(foregroundCallbackCalled)
    }
}
