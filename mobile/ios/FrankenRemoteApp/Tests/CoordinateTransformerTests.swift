//
// CoordinateTransformerTests.swift — Unit tests for coordinate transformations
//

import XCTest
@testable import FrankenRemoteApp

final class CoordinateTransformerTests: XCTestCase {
    func testAspectFitCalculation() {
        let transformer = CoordinateTransformer(
            desktopWidth: 1920,
            desktopHeight: 1080,
            viewportSize: CGSize(width: 393, height: 852),
            zoomScale: 1.0,
            panOffset: .zero
        )

        let rect = transformer.fittedRect
        XCTAssertEqual(rect.width, 393, accuracy: 0.1)
        XCTAssertEqual(rect.height, 393 / (1920.0 / 1080.0), accuracy: 0.1)
        XCTAssertGreaterThan(rect.origin.y, 0)
    }

    func testViewPointToDesktopPointMapping() {
        let transformer = CoordinateTransformer(
            desktopWidth: 1920,
            desktopHeight: 1080,
            viewportSize: CGSize(width: 1920, height: 1080),
            zoomScale: 1.0,
            panOffset: .zero
        )

        // Center point
        let centerPoint = CGPoint(x: 960, y: 540)
        let mappedCenter = transformer.viewPointToDesktopPoint(centerPoint)
        XCTAssertNotNil(mappedCenter)
        XCTAssertEqual(mappedCenter!.x, 960)
        XCTAssertEqual(mappedCenter!.y, 540)

        // Top-left origin
        let originPoint = CGPoint(x: 0, y: 0)
        let mappedOrigin = transformer.viewPointToDesktopPoint(originPoint)
        XCTAssertNotNil(mappedOrigin)
        XCTAssertEqual(mappedOrigin!.x, 0)
        XCTAssertEqual(mappedOrigin!.y, 0)

        // Point outside active bounds returns nil
        let outsidePoint = CGPoint(x: -10, y: 500)
        XCTAssertNil(transformer.viewPointToDesktopPoint(outsidePoint))
    }

    func testTrackpadDeltaClamping() {
        let transformer = CoordinateTransformer(
            desktopWidth: 1920,
            desktopHeight: 1080,
            viewportSize: CGSize(width: 1920, height: 1080),
            zoomScale: 1.0,
            panOffset: .zero
        )

        let initial = (x: Int32(100), y: Int32(100))
        let delta = CGPoint(x: 50, y: 20)
        let result = transformer.applyTrackpadDelta(currentDesktop: initial, deltaView: delta, sensitivity: 1.0)

        XCTAssertEqual(result.x, 150)
        XCTAssertEqual(result.y, 120)

        // Clamp at bounds
        let bigDelta = CGPoint(x: 5000, y: 5000)
        let clampedResult = transformer.applyTrackpadDelta(currentDesktop: initial, deltaView: bigDelta, sensitivity: 1.0)
        XCTAssertEqual(clampedResult.x, 1919)
        XCTAssertEqual(clampedResult.y, 1079)
    }
}
