import XCTest
@testable import FrankenRemote

final class PictureMapperTests: XCTestCase {
    /// A 2560x1440 host on a 393x852 portrait phone with notch insets.
    private func portrait() -> PictureMapper {
        PictureMapper(viewSize: CGSize(width: 393, height: 852),
                      insets: Insets(top: 59, left: 0, bottom: 34, right: 0),
                      picture: CGSize(width: 2560, height: 1440))
    }

    func testFitLetterboxesInsideTheSafeArea() {
        let m = portrait()
        XCTAssertEqual(m.fitScale, 393.0 / 2560.0, accuracy: 1e-9)
        let rect = m.pictureRect
        XCTAssertEqual(rect.width, 393, accuracy: 1e-9)
        XCTAssertEqual(rect.height, 1440 * 393.0 / 2560.0, accuracy: 1e-9)
        // Centred vertically in the safe area, not the whole view.
        XCTAssertEqual(rect.midY, 59 + (852 - 59 - 34) / 2.0, accuracy: 1e-9)
    }

    func testMappingRoundTripsAndRefusesOutsidePointsWithoutClamping() throws {
        let m = portrait()
        let rect = m.pictureRect
        let inside = CGPoint(x: rect.midX, y: rect.midY)
        let p = try XCTUnwrap(m.toPicture(inside))
        XCTAssertEqual(p.x, 1280, accuracy: 1e-6)
        XCTAssertEqual(p.y, 720, accuracy: 1e-6)
        let back = m.toView(p)
        XCTAssertEqual(back.x, inside.x, accuracy: 1e-9)
        XCTAssertEqual(back.y, inside.y, accuracy: 1e-9)
        // The letterbox bars and the exact right/bottom edges are outside.
        XCTAssertNil(m.toPicture(CGPoint(x: 100, y: rect.minY - 0.5)))
        XCTAssertNil(m.toPicture(CGPoint(x: 100, y: rect.maxY)))
        XCTAssertNil(m.toPicture(CGPoint(x: rect.maxX, y: rect.midY)))
        XCTAssertNil(m.toPicture(CGPoint(x: -1, y: rect.midY)))
        // Positive control: just inside the left edge maps to column 0.
        XCTAssertEqual(m.toPicture(CGPoint(x: 0, y: rect.midY))?.x, 0)
    }

    func testPinchZoomKeepsThePointUnderTheFingers() {
        var m = portrait()
        let anchor = CGPoint(x: 300, y: 420)
        let before = m.toPictureUnbounded(anchor)
        // Zoomed until the picture overfills both axes: the point stays put.
        m.zoom(by: 6, at: anchor)
        XCTAssertEqual(m.zoom, 6, accuracy: 1e-9)
        let after = m.toPictureUnbounded(anchor)
        XCTAssertEqual(after.x, before.x, accuracy: 1e-6)
        XCTAssertEqual(after.y, before.y, accuracy: 1e-6)
    }

    func testAnAxisThatStillFitsStaysCentred() {
        var m = portrait()
        let anchor = CGPoint(x: 300, y: 420)
        let before = m.toPictureUnbounded(anchor)
        m.zoom(by: 3, at: anchor)
        // Width overfills (follows the fingers); height still fits (centred).
        XCTAssertEqual(m.toPictureUnbounded(anchor).x, before.x, accuracy: 1e-6)
        XCTAssertEqual(m.pictureRect.midY, m.content.midY, accuracy: 1e-9)
    }

    func testZoomIsBoundedAndPanNeverUncoversTheContentArea() {
        var m = portrait()
        m.zoom(by: 0.2, at: CGPoint(x: 200, y: 400))
        XCTAssertEqual(m.zoom, 1, "never below fit")
        m.zoom(by: 1000, at: CGPoint(x: 200, y: 400))
        XCTAssertEqual(m.scale, PictureMapper.maxPointsPerPixel, accuracy: 1e-9)
        m.setZoom(4, at: CGPoint(x: 196, y: 400))
        m.pan(by: CGVector(dx: 100_000, dy: 0))
        // The picture's left edge stops at the content's left edge.
        XCTAssertEqual(m.pictureRect.minX, m.content.minX, accuracy: 1e-6)
        m.pan(by: CGVector(dx: -100_000, dy: 0))
        XCTAssertEqual(m.pictureRect.maxX, m.content.maxX, accuracy: 1e-6)
    }

    func testActualPixelsIsOneDevicePixelPerPicturePixel() {
        var m = portrait()
        let exact = m.actualPixelsZoom(screenScale: 3)
        m.setZoom(exact, at: CGPoint(x: 196, y: 400))
        XCTAssertEqual(m.devicePixelsPerPixel(screenScale: 3), 1, accuracy: 1e-9)
    }

    func testRotationKeepsZoomAndTheCentredPixel() {
        var m = portrait()
        m.setZoom(2, at: CGPoint(x: 196, y: 455))
        let centre = m.toPictureUnbounded(CGPoint(x: m.content.midX, y: m.content.midY))
        m.viewSize = CGSize(width: 852, height: 393)
        m.insets = Insets(top: 0, left: 59, bottom: 21, right: 59)
        m.clamp()
        XCTAssertEqual(m.zoom, 2)
        let rotated = m.toPictureUnbounded(CGPoint(x: m.content.midX, y: m.content.midY))
        // Same picture pixel in the middle unless clamping had to move it.
        XCTAssertEqual(rotated.x, centre.x, accuracy: 1e-6)
    }

    /// A 1920x1080 host on a 2556x1179-pixel phone in landscape (393 points
    /// tall at 3x): fit is 1.09 device pixels per picture pixel.
    private func landscape() -> PictureMapper {
        var m = PictureMapper(picture: CGSize(width: 1920, height: 1080))
        m.setGeometry(viewSize: CGSize(width: 852, height: 393),
                      insets: Insets(top: 0, left: 59, bottom: 0, right: 59), deviceScale: 3)
        return m
    }

    func testAnUntouchedViewRestsAtWholeDevicePixelsWhenThatCostsLittle() {
        let m = landscape()
        // Planted negative: plain fit is an off-integer upscale (blurred).
        XCTAssertEqual(m.fitScale * 3, 1179.0 / 1080.0, accuracy: 1e-9)
        XCTAssertEqual(m.zoom, m.restingZoom)
        XCTAssertLessThan(m.zoom, 1)
        XCTAssertEqual(m.devicePixelsPerPixel(screenScale: 3), 1, accuracy: 1e-9)
        // Its origin is on the device pixel grid.
        let r = m.pictureRect
        XCTAssertEqual((r.minX * 3).rounded(), r.minX * 3, accuracy: 1e-9)
        XCTAssertEqual((r.minY * 3).rounded(), r.minY * 3, accuracy: 1e-9)
        XCTAssertTrue(m.resting)
        // Far from a whole number (iPad-like: 1.42), it rests at fit.
        var pad = PictureMapper(picture: CGSize(width: 1920, height: 1080))
        pad.setGeometry(viewSize: CGSize(width: 1366, height: 1024), insets: Insets(), deviceScale: 2)
        XCTAssertEqual(pad.zoom, 1)
    }

    func testActualPixelsIsReachableBelowFitWhenTheDeviceHasMorePixels() {
        var pad = PictureMapper(picture: CGSize(width: 1920, height: 1080))
        pad.setGeometry(viewSize: CGSize(width: 1366, height: 1024), insets: Insets(), deviceScale: 2)
        let exact = pad.actualPixelsZoom(screenScale: 2)
        // Planted negative: the old floor of 1 made this impossible.
        XCTAssertLessThan(exact, 1)
        pad.setZoom(exact, at: CGPoint(x: 683, y: 512))
        XCTAssertEqual(pad.devicePixelsPerPixel(screenScale: 2), 1, accuracy: 1e-9)
        XCTAssertFalse(pad.resting)
        pad.zoom(by: 0.1, at: CGPoint(x: 683, y: 512))
        XCTAssertEqual(pad.zoom, pad.minZoom, accuracy: 1e-12, "never below one device pixel")
        // Rotation keeps a chosen zoom; an untouched view re-rests.
        pad.setGeometry(viewSize: CGSize(width: 1024, height: 1366), insets: Insets(), deviceScale: 2)
        XCTAssertEqual(pad.devicePixelsPerPixel(screenScale: 2), 1, accuracy: 1e-9)
        var phone = landscape()
        phone.setGeometry(viewSize: CGSize(width: 393, height: 852),
                          insets: Insets(top: 59, left: 0, bottom: 34, right: 0), deviceScale: 3)
        XCTAssertEqual(phone.zoom, phone.restingZoom)
    }

    func testRevealPansJustEnoughToShowAPoint() {
        var m = portrait()
        m.setZoom(4, at: CGPoint(x: 196, y: 455))
        let far = CGPoint(x: 2000, y: 720)
        XCTAssertTrue(m.reveal(far, margin: 44))
        XCTAssertEqual(m.toView(far).x, m.content.maxX - 44, accuracy: 1e-6)
        // Already visible: no pan.
        XCTAssertFalse(m.reveal(far, margin: 44))
        // Near the picture's own edge it pans only as far as that edge.
        let edge = CGPoint(x: 2555, y: 720)
        XCTAssertTrue(m.reveal(edge, margin: 44))
        XCTAssertEqual(m.pictureRect.maxX, m.content.maxX, accuracy: 1e-6)
    }
}
