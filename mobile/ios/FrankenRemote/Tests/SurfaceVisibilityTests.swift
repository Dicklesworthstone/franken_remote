import MetalKit
import XCTest
@testable import FrankenRemote

final class SurfaceVisibilityTests: XCTestCase {
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
