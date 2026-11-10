import XCTest
@testable import FrankenRemote

final class TouchInterpreterTests: XCTestCase {
    private var sink = RecordingSink()
    private var touch = TouchInterpreter()

    /// A 1920x1080 picture fitted into a 400x800 view at exactly 0.2
    /// points per pixel, letterboxed vertically (picture y 292...508).
    override func setUp() {
        sink = RecordingSink()
        touch = TouchInterpreter()
        touch.mapper = PictureMapper(viewSize: CGSize(width: 384, height: 800),
                                     picture: CGSize(width: 1920, height: 1080))
        touch.sink = sink
        touch.screenScale = 3
    }

    private var middle: CGPoint { CGPoint(x: 192, y: 400) }

    // MARK: Trackpad

    func testTrackpadTapClicksAtThePointerNotTheFinger() {
        touch.controlling = true
        _ = touch.began(1, at: CGPoint(x: 20, y: 300), time: 0)
        _ = touch.ended(1, at: CGPoint(x: 21, y: 301), time: 0.1)
        let centre = CGPoint(x: 960, y: 540)
        XCTAssertEqual(sink.buttons, [.button(.primary, true, centre), .button(.primary, false, centre)])
        // A still press is a tap right up to the hold (no dead band)...
        sink.calls.removeAll()
        _ = touch.began(2, at: middle, time: 1)
        _ = touch.ended(2, at: middle, time: 1.45)
        XCTAssertEqual(sink.buttons.count, 2)
        // ...and planted negative: one held past it is not.
        sink.calls.removeAll()
        _ = touch.began(3, at: middle, time: 2)
        _ = touch.ended(3, at: middle, time: 2.6)
        XCTAssertTrue(sink.buttons.isEmpty)
    }

    func testTrackpadMovesRelativelyWithAccelerationAndFollowsOneToOneWhenSlow() {
        touch.controlling = true
        _ = touch.began(1, at: middle, time: 0)
        // Inside the slop nothing moves.
        _ = touch.moved(1, to: CGPoint(x: 197, y: 400), time: 0.05)
        XCTAssertTrue(sink.pointers.isEmpty)
        // Slow: 20 points over a second at 0.2 points/pixel = 100 pixels
        // (the whole travel from the touch-down point, none lost in the slop).
        _ = touch.moved(1, to: CGPoint(x: 212, y: 400), time: 1.0)
        XCTAssertEqual(sink.pointers.last?.x ?? 0, 960 + 100, accuracy: 1e-6)
        // Fast: 20 points in 10 ms goes further than 1:1.
        _ = touch.moved(1, to: CGPoint(x: 232, y: 400), time: 1.01)
        XCTAssertGreaterThan((sink.pointers.last?.x ?? 0) - 1060, 100 * 1.5)
        _ = touch.ended(1, at: CGPoint(x: 232, y: 400), time: 1.02)
        XCTAssertTrue(sink.buttons.isEmpty, "a move is never a click")
    }

    func testThePointerStaysInsideThePicture() throws {
        touch.controlling = true
        _ = touch.began(1, at: middle, time: 0)
        _ = touch.moved(1, to: CGPoint(x: 192 + 1000, y: 400 + 1000), time: 0.5)
        let last = try XCTUnwrap(sink.pointers.last)
        XCTAssertLessThan(last.x, 1920)
        XCTAssertLessThan(last.y, 1080)
        XCTAssertGreaterThan(last.x, 1919)
    }

    func testTouchAndHoldThenDragDragsAndLiftReleases() {
        touch.controlling = true
        _ = touch.began(1, at: middle, time: 0)
        XCTAssertEqual(touch.tick(time: 0.2), [])
        XCTAssertEqual(touch.tick(time: 0.45), [])
        XCTAssertEqual(touch.tick(time: 0.51), [.haptic])
        XCTAssertEqual(sink.buttons, [.button(.primary, true, CGPoint(x: 960, y: 540))])
        _ = touch.moved(1, to: CGPoint(x: 202, y: 400), time: 1.36)
        XCTAssertEqual(sink.pointers.last?.x ?? 0, 1010, accuracy: 1e-6)
        _ = touch.ended(1, at: CGPoint(x: 202, y: 400), time: 1.4)
        XCTAssertEqual(sink.buttons.last, .button(.primary, false, CGPoint(x: 1010, y: 540)))
    }

    func testAHoldDragStartsFromWhereTheFingerCreptNotWhereItLanded() {
        touch.controlling = true
        _ = touch.began(1, at: middle, time: 0)
        // Creeps 8 points (inside the slop: still a still touch).
        _ = touch.moved(1, to: CGPoint(x: 200, y: 400), time: 0.3)
        XCTAssertTrue(sink.pointers.isEmpty)
        XCTAssertEqual(touch.tick(time: 0.51), [.haptic])
        // Planted negative (the old rule measured from the landing point):
        // 2 more points is 10 pixels, not 50.
        _ = touch.moved(1, to: CGPoint(x: 202, y: 400), time: 1.5)
        XCTAssertEqual(sink.pointers.last?.x ?? 0, 970, accuracy: 1e-6)
    }

    func testTheTrackpadPointerStartsAtASeededHostCursor() {
        touch.controlling = true
        XCTAssertFalse(touch.hasPointer)
        touch.seedPointer(CGPoint(x: 100, y: 200))
        XCTAssertTrue(touch.hasPointer)
        _ = touch.began(1, at: middle, time: 0)
        _ = touch.ended(1, at: middle, time: 0.1)
        let at = CGPoint(x: 100, y: 200)
        XCTAssertEqual(sink.buttons, [.button(.primary, true, at), .button(.primary, false, at)])
        // Off-picture seeds are refused; a reset forgets the pointer.
        touch.reset()
        touch.seedPointer(CGPoint(x: 1920, y: 10))
        XCTAssertFalse(touch.hasPointer)
    }

    func testTwoFingerTapRightClicksAndDoubleTapDoubleClicks() {
        touch.controlling = true
        _ = touch.began(1, at: CGPoint(x: 150, y: 400), time: 0)
        _ = touch.began(2, at: CGPoint(x: 230, y: 400), time: 0.02)
        _ = touch.ended(1, at: CGPoint(x: 150, y: 400), time: 0.1)
        _ = touch.ended(2, at: CGPoint(x: 230, y: 400), time: 0.12)
        let centre = CGPoint(x: 960, y: 540)
        XCTAssertEqual(sink.buttons, [.button(.secondary, true, centre), .button(.secondary, false, centre)])
        sink.calls.removeAll()
        for start in [1.0, 1.2] {
            _ = touch.began(3, at: middle, time: start)
            _ = touch.ended(3, at: middle, time: start + 0.08)
        }
        XCTAssertEqual(sink.buttons.count, 4, "two clicks in quick succession")
    }

    func testTwoFingerScrollIsNaturalWithRemainderAndMomentum() {
        touch.controlling = true
        touch.scroller.pointsPerLine = 20
        _ = touch.began(1, at: CGPoint(x: 150, y: 500), time: 0)
        _ = touch.began(2, at: CGPoint(x: 230, y: 500), time: 0.01)
        // Fingers move up 60 points: content follows, so scroll down 3 lines
        // in total (the first 10 points decide the gesture and are sent).
        for (n, y) in [490.0, 470.0, 440.0].enumerated() {
            let t = 0.02 + Double(n) * 0.016
            _ = touch.moved(1, to: CGPoint(x: 150, y: y), time: t)
            _ = touch.moved(2, to: CGPoint(x: 230, y: y), time: t)
        }
        let total = sink.scrolls.reduce(0) { $0 + $1.1 }
        XCTAssertEqual(total, 3, accuracy: 1e-9)
        XCTAssertTrue(sink.scrolls.allSatisfy { $0.1 >= 0 }, "finger up scrolls down")
        // A flick keeps going after the fingers lift, then stops by itself.
        _ = touch.ended(1, at: CGPoint(x: 150, y: 440), time: 0.06)
        _ = touch.ended(2, at: CGPoint(x: 230, y: 440), time: 0.06)
        let before = sink.scrolls.count
        var t = 0.06
        _ = touch.tick(time: t)
        for _ in 0..<240 {
            t += 1.0 / 60
            _ = touch.tick(time: t)
        }
        XCTAssertGreaterThan(sink.scrolls.count, before)
        XCTAssertEqual(sink.calls.last, .scrollEnded)
        XCTAssertTrue(touch.isIdle)
    }

    func testANewTouchStopsMomentum() {
        touch.controlling = true
        _ = touch.began(1, at: CGPoint(x: 150, y: 500), time: 0)
        _ = touch.began(2, at: CGPoint(x: 230, y: 500), time: 0.01)
        for (n, y) in [480.0, 440.0, 380.0].enumerated() {
            let t = 0.02 + Double(n) * 0.016
            _ = touch.moved(1, to: CGPoint(x: 150, y: y), time: t)
            _ = touch.moved(2, to: CGPoint(x: 230, y: y), time: t)
        }
        _ = touch.ended(1, at: CGPoint(x: 150, y: 380), time: 0.06)
        _ = touch.ended(2, at: CGPoint(x: 230, y: 380), time: 0.06)
        XCTAssertTrue(touch.scroller.coasting)
        _ = touch.began(3, at: middle, time: 0.1)
        XCTAssertFalse(touch.scroller.coasting)
        XCTAssertEqual(sink.calls.last, .scrollEnded)
    }

    func testPinchZoomsTheViewAndSendsNothing() {
        touch.controlling = true
        _ = touch.began(1, at: CGPoint(x: 150, y: 400), time: 0)
        _ = touch.began(2, at: CGPoint(x: 234, y: 400), time: 0.01)
        _ = touch.moved(1, to: CGPoint(x: 108, y: 400), time: 0.05)
        _ = touch.moved(2, to: CGPoint(x: 276, y: 400), time: 0.05)
        XCTAssertEqual(touch.mapper.zoom, 2, accuracy: 1e-6)
        XCTAssertTrue(sink.calls.isEmpty)
    }

    func testAOneFingerPinchZoomsAndNeverScrolls() {
        touch.controlling = true
        _ = touch.began(1, at: CGPoint(x: 150, y: 400), time: 0)
        _ = touch.began(2, at: CGPoint(x: 234, y: 400), time: 0.01)
        // One finger still, the other spreading in 2-point steps: the
        // centroid travels half as far as the spread. Planted negative: the
        // old rule called this a scroll once the travel passed the slop.
        for step in 1...30 {
            _ = touch.moved(2, to: CGPoint(x: 234 + CGFloat(step) * 2, y: 400), time: 0.01 + Double(step) * 0.008)
        }
        XCTAssertTrue(sink.scrolls.isEmpty)
        XCTAssertGreaterThan(touch.mapper.zoom, 1.3)
        // And lifting after it moved is never a two-finger right click.
        _ = touch.ended(1, at: CGPoint(x: 150, y: 400), time: 0.26)
        _ = touch.ended(2, at: CGPoint(x: 294, y: 400), time: 0.27)
        XCTAssertTrue(sink.buttons.isEmpty)
    }

    // MARK: Direct

    func testDirectTapClicksWhereTouchedAndOutsideTouchesSendNothing() {
        touch.controlling = true
        touch.mode = .direct
        _ = touch.began(1, at: CGPoint(x: 96, y: 346), time: 0)
        _ = touch.ended(1, at: CGPoint(x: 96, y: 346), time: 0.1)
        let p = CGPoint(x: 480, y: 270)
        XCTAssertEqual(sink.calls, [.pointer(p), .button(.primary, true, p), .button(.primary, false, p)])
        // The letterbox bar is outside the picture: nothing at all is sent,
        // not a click moved onto the picture's edge.
        sink.calls.removeAll()
        _ = touch.began(2, at: CGPoint(x: 96, y: 100), time: 1)
        _ = touch.ended(2, at: CGPoint(x: 96, y: 100), time: 1.1)
        XCTAssertTrue(sink.calls.isEmpty)
    }

    func testDirectDragReleasesAtTheLastInsidePoint() {
        touch.controlling = true
        touch.mode = .direct
        _ = touch.began(1, at: CGPoint(x: 96, y: 346), time: 0)
        _ = touch.moved(1, to: CGPoint(x: 120, y: 346), time: 0.1)
        _ = touch.moved(1, to: CGPoint(x: 120, y: 200), time: 0.2) // into the bar
        _ = touch.ended(1, at: CGPoint(x: 120, y: 150), time: 0.3)
        let start = CGPoint(x: 480, y: 270)
        let inside = CGPoint(x: 600, y: 270)
        XCTAssertEqual(sink.buttons, [.button(.primary, true, start), .button(.primary, false, inside)])
        XCTAssertEqual(sink.pointers.last, inside, "no pointer event outside the picture")
    }

    func testDirectLongPressRightClicks() {
        touch.controlling = true
        touch.mode = .direct
        _ = touch.began(1, at: CGPoint(x: 96, y: 346), time: 0)
        XCTAssertEqual(touch.tick(time: 0.51), [.haptic])
        let p = CGPoint(x: 480, y: 270)
        XCTAssertEqual(sink.buttons, [.button(.secondary, true, p), .button(.secondary, false, p)])
        _ = touch.ended(1, at: CGPoint(x: 96, y: 346), time: 0.7)
        XCTAssertEqual(sink.buttons.count, 2, "lifting after a long press sends nothing more")
    }

    // MARK: Viewing and control changes

    func testWithoutControlTouchesOnlyMoveTheView() {
        _ = touch.began(1, at: middle, time: 0)
        _ = touch.ended(1, at: middle, time: 0.05)
        _ = touch.began(1, at: middle, time: 0.15)
        XCTAssertEqual(touch.ended(1, at: middle, time: 0.2), [.viewport])
        XCTAssertGreaterThan(touch.mapper.zoom, 1, "double tap zooms")
        XCTAssertTrue(sink.calls.isEmpty)
    }

    func testLosingControlMidDragReleasesTheButton() {
        touch.controlling = true
        _ = touch.began(1, at: middle, time: 0)
        _ = touch.tick(time: 0.51)
        touch.controlling = false
        XCTAssertEqual(sink.buttons.last, .button(.primary, false, CGPoint(x: 960, y: 540)))
        // The rest of that touch does nothing.
        sink.calls.removeAll()
        _ = touch.moved(1, to: CGPoint(x: 250, y: 400), time: 0.5)
        _ = touch.ended(1, at: CGPoint(x: 250, y: 400), time: 0.6)
        XCTAssertTrue(sink.calls.isEmpty)
    }
}
