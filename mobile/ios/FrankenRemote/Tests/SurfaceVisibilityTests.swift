import MetalKit
import XCTest
@testable import FrankenRemote

final class SurfaceVisibilityTests: XCTestCase {
    @MainActor
    func testRefreshRequestsTrackCurrentPowerPressureAndTheAttachedDisplay() {
        let rate = RemoteSurfaceView.preferredRefreshRate(maximum:thermal:lowPower:)
        XCTAssertEqual(rate(120, .nominal, false), 120)
        XCTAssertEqual(rate(120, .fair, false), 60)
        XCTAssertEqual(rate(60, .serious, false), 30)
        XCTAssertEqual(rate(120, .nominal, true), 30)
        XCTAssertEqual(rate(120, .fair, true), 30)
        XCTAssertEqual(rate(120, .critical, true), 15)
        XCTAssertEqual(rate(120, .critical, false), 15)
        XCTAssertEqual(rate(24, .serious, true), 24)
        XCTAssertEqual(rate(60, .nominal, false), 60)
    }

    @MainActor
    func testPowerNotificationReappliesPolicyToTheReplacementMetalView() async throws {
        let view = RemoteSurfaceView(frame: CGRect(x: 0, y: 0, width: 320, height: 640))
        view.setSurfaceVisible(false)
        view.setSurfaceVisible(true)
        let metal = try XCTUnwrap(view.subviews.compactMap { $0 as? MTKView }.first)
        let info = ProcessInfo.processInfo
        let expected = RemoteSurfaceView.preferredRefreshRate(
            maximum: 60, thermal: info.thermalState, lowPower: info.isLowPowerModeEnabled)
        XCTAssertEqual(metal.preferredFramesPerSecond, expected)
        for name in [Notification.Name.NSProcessInfoPowerStateDidChange,
                     ProcessInfo.thermalStateDidChangeNotification] {
            metal.preferredFramesPerSecond = 10
            NotificationCenter.default.post(name: name, object: info)
            for _ in 0..<50 {
                if metal.preferredFramesPerSecond == expected { break }
                try await Task.sleep(for: .milliseconds(20))
            }
            XCTAssertEqual(metal.preferredFramesPerSecond, expected)
        }
        withExtendedLifetime(view) {}
    }

    @MainActor
    func testCoveredDesktopNeverReusesItsRetainedDrawableOnReturn() throws {
        let view = RemoteSurfaceView(frame: CGRect(x: 0, y: 0, width: 320, height: 640))
        func surface() throws -> MTKView {
            let surfaces = view.subviews.compactMap { $0 as? MTKView }
            XCTAssertEqual(surfaces.count, 1)
            return try XCTUnwrap(surfaces.first)
        }
        view.setSurfaceVisible(true)
        let first = try surface()
        view.setSurfaceVisible(true)
        XCTAssertTrue(try surface() === first)

        view.setSurfaceVisible(false)
        XCTAssertTrue(first.isHidden)
        view.setSurfaceVisible(false)
        XCTAssertTrue(try surface() === first)

        view.setSurfaceVisible(true)
        let next = try surface()
        XCTAssertFalse(next === first)
        XCTAssertNil(first.superview)
        XCTAssertNil(first.delegate)
        XCTAssertFalse(next.isHidden)
        XCTAssertEqual(next.clearColor.alpha, 1)
        view.setSurfaceVisible(true)
        XCTAssertTrue(try surface() === next)
    }
}
