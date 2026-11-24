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

    func testZoomedDirectInputStopsAtEveryVisibleContentEdge() {
        var m = PictureMapper(viewSize: CGSize(width: 400, height: 600),
                              insets: Insets(top: 20, left: 30, bottom: 200, right: 40),
                              picture: CGSize(width: 1920, height: 1080))
        m.setZoom(4, at: CGPoint(x: m.content.midX, y: m.content.midY))
        for outside in [CGPoint(x: 29, y: 200), CGPoint(x: 360, y: 200),
                        CGPoint(x: 200, y: 19), CGPoint(x: 200, y: 400)] {
            XCTAssertTrue(m.containsPicture(m.toPictureUnbounded(outside)))
            XCTAssertNil(m.toPicture(outside))
        }
        XCTAssertNotNil(m.toPicture(CGPoint(x: 30, y: 20)))
        XCTAssertNotNil(m.toPicture(CGPoint(x: 359.5, y: 399.5)))
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

    func testScaledPictureUsesOneOriginalDisplayRectangleForImageInputAndCursor() throws {
        let m = PictureMapper(viewSize: CGSize(width: 1582, height: 888),
                              picture: CGSize(width: 1582, height: 888),
                              display: CGSize(width: 5120, height: 2880))
        let image = CGRect(x: 2, y: 0, width: 1578, height: 888)
        XCTAssertEqual(m.imageBounds, image)
        XCTAssertEqual(m.imageRect, image)
        XCTAssertNil(m.toPicture(CGPoint(x: 1, y: 444)))
        XCTAssertNil(m.toPicture(CGPoint(x: 1580, y: 444)))
        XCTAssertEqual(m.toPicture(CGPoint(x: 2, y: 444)), CGPoint(x: 2, y: 444))

        let cursor = try XCTUnwrap(m.picturePoint(forDisplay: CGPoint(x: 2560, y: 1440)))
        XCTAssertEqual(cursor, CGPoint(x: 791, y: 444))
        let shape = try XCTUnwrap(m.cursorRect(
            at: cursor, size: CGSize(width: 64, height: 32), hotspot: CGPoint(x: 8, y: 4)))
        XCTAssertEqual(shape.width, 64 * 1578.0 / 5120, accuracy: 1e-9)
        XCTAssertEqual(shape.height, 32 * 888.0 / 2880, accuracy: 1e-9)
        XCTAssertEqual(shape.minX + 8 * 1578.0 / 5120, m.toView(cursor).x, accuracy: 1e-9)
        XCTAssertEqual(shape.minY + 4 * 888.0 / 2880, m.toView(cursor).y, accuracy: 1e-9)
        XCTAssertLessThan(shape.width, 64, "a source cursor is not an encoded-size cursor")
    }

    func testScaledPictureZoomAndPanKeepTheInputAnchorAndImageVisible() throws {
        var m = PictureMapper(viewSize: CGSize(width: 800, height: 500),
                              picture: CGSize(width: 1582, height: 888),
                              display: CGSize(width: 5120, height: 2880))
        let anchor = CGPoint(x: 500, y: 300)
        let before = try XCTUnwrap(m.toPicture(anchor))
        m.zoom(by: 3, at: anchor)
        let after = try XCTUnwrap(m.toPicture(anchor))
        XCTAssertEqual(after.x, before.x, accuracy: 1e-6)
        XCTAssertEqual(after.y, before.y, accuracy: 1e-6)
        for direction in [CGFloat(1), -1] {
            m.pan(by: CGVector(dx: direction * 100_000, dy: direction * 100_000))
            XCTAssertFalse(m.imageRect.intersection(m.content).isEmpty)
            let visible = m.visiblePicture
            XCTAssertTrue(m.containsPicture(CGPoint(x: visible.midX, y: visible.midY)))
        }
    }

    func testExtremeAspectRatioCannotPanTheActualImageOutOfView() {
        var m = PictureMapper(viewSize: CGSize(width: 400, height: 600),
                              picture: CGSize(width: 500, height: 2),
                              display: CGSize(width: 8192, height: 64))
        XCTAssertEqual(m.imageBounds, CGRect(x: 122, y: 0, width: 256, height: 2))
        XCTAssertEqual(m.imageRect.width, 400, accuracy: 1e-9)
        m.setZoom(1000, at: CGPoint(x: 200, y: 300))
        for direction in [CGFloat(1), -1] {
            m.pan(by: CGVector(dx: direction * 100_000, dy: direction * 100_000))
            let visible = m.imageRect.intersection(m.content)
            XCTAssertFalse(visible.isNull)
            XCTAssertFalse(visible.isEmpty)
            XCTAssertEqual(visible.width, 400, accuracy: 1e-6)
            let pixels = m.visiblePicture
            XCTAssertTrue(m.containsPicture(CGPoint(x: pixels.midX, y: pixels.midY)))
        }
    }

    func testFrameUpdatesPreserveZoomButMissingOrChangedDisplayRetiresGeometry() {
        var m = PictureMapper(viewSize: CGSize(width: 400, height: 800),
                              picture: CGSize(width: 640, height: 358),
                              display: CGSize(width: 1365, height: 767))
        XCTAssertEqual(m.imageBounds, CGRect(x: 1, y: 0, width: 637, height: 358))
        m.setPicture(m.picture, display: m.display, frame: 10)
        m.setZoom(3, at: CGPoint(x: 200, y: 400))
        let center = m.center
        m.setPicture(m.picture, display: m.display, frame: 11)
        XCTAssertEqual(m.frame, 11)
        XCTAssertEqual(m.zoom, 3)
        XCTAssertEqual(m.center, center)
        m.setPicture(m.picture, display: .zero, frame: 12)
        XCTAssertFalse(m.isReady)
        XCTAssertNil(m.toPicture(CGPoint(x: 200, y: 400)))
        XCTAssertNil(m.picturePoint(forDisplay: CGPoint(x: 100, y: 100)))
        m.setPicture(CGSize(width: 640, height: 358))
        XCTAssertEqual(m.frame, 0)
        XCTAssertEqual(m.display, m.picture, "legacy native geometry remains exact")
        XCTAssertEqual(m.imageBounds, CGRect(x: 0, y: 0, width: 640, height: 358))
    }

}
