import XCTest
@testable import FrankenRemote

final class KeyboardTests: XCTestCase {
    func testPrintableAsciiMapsToUsLayoutKeysWithShift() {
        XCTAssertEqual(KeyboardMap.stroke(for: "a"), KeyStroke(usage: 0x04, shift: false))
        XCTAssertEqual(KeyboardMap.stroke(for: "Z"), KeyStroke(usage: 0x1D, shift: true))
        XCTAssertEqual(KeyboardMap.stroke(for: "1"), KeyStroke(usage: 0x1E, shift: false))
        XCTAssertEqual(KeyboardMap.stroke(for: "0"), KeyStroke(usage: 0x27, shift: false))
        XCTAssertEqual(KeyboardMap.stroke(for: "!"), KeyStroke(usage: 0x1E, shift: true))
        XCTAssertEqual(KeyboardMap.stroke(for: ")"), KeyStroke(usage: 0x27, shift: true))
        XCTAssertEqual(KeyboardMap.stroke(for: "?"), KeyStroke(usage: 0x38, shift: true))
        XCTAssertEqual(KeyboardMap.stroke(for: "\n"), KeyStroke(usage: 0x28, shift: false))
        XCTAssertEqual(KeyboardMap.stroke(for: " "), KeyStroke(usage: 0x2C, shift: false))
        // Every printable ASCII character has exactly one key.
        for value in 0x20...0x7E {
            let character = Character(UnicodeScalar(UInt8(value)))
            XCTAssertNotNil(KeyboardMap.stroke(for: character), "\(character)")
        }
    }

    func testNonAsciiIsNeverGuessed() {
        for character: Character in ["é", "ß", "€", "’", "“", "—", "😀", "ñ", "日"] {
            XCTAssertNil(KeyboardMap.stroke(for: character), "\(character)")
        }
    }

    func testStickyModifiersLatchLockAndRelease() {
        var m = StickyModifiers()
        m.tap(.command)
        XCTAssertEqual(m.level(.command), .latched)
        m.consume()
        XCTAssertEqual(m.level(.command), .off, "a latched modifier is used up by one key")
        m.tap(.control)
        m.tap(.control)
        XCTAssertEqual(m.level(.control), .locked)
        m.consume()
        XCTAssertEqual(m.level(.control), .locked, "locked stays")
        m.tap(.control)
        XCTAssertEqual(m.level(.control), .off)
        m.tap(.shift)
        m.tap(.option)
        XCTAssertEqual(m.active, [.option, .shift])
    }

    func testTypingPressesModifiersAroundTheKeyAndAlwaysReleasesThem() {
        let sink = RecordingSink()
        let result = KeyTyper.type(0x06, modifiers: [HIDUsage.leftGUI, HIDUsage.leftShift], into: sink)
        XCTAssertEqual(result, .sent)
        XCTAssertEqual(sink.calls, [
            .key(0xE3, true), .key(0xE1, true), .key(0x06, true), .key(0x06, false),
            .key(0xE1, false), .key(0xE3, false),
        ])
        // A refused key still releases what was pressed.
        let refusing = RecordingSink()
        refusing.keyResult = .controlLost
        XCTAssertEqual(KeyTyper.type(0x06, modifiers: [], into: refusing), .controlLost)
        XCTAssertEqual(refusing.calls, [.key(0x06, true)])
    }

    func testOnlyUsagesTheHostAcceptsAreSendable() {
        XCTAssertTrue(HIDUsage.isSendable(0x04))
        XCTAssertTrue(HIDUsage.isSendable(0xA4))
        XCTAssertTrue(HIDUsage.isSendable(0xE7))
        for usage: UInt16 in [0, 1, 3, 0xA5, 0xDF, 0xE8] {
            XCTAssertFalse(HIDUsage.isSendable(usage), "\(usage)")
        }
        XCTAssertEqual(HIDUsage.function(12), 0x45)
        XCTAssertNil(HIDUsage.function(13))
    }
}

final class AccelerationAndScrollTests: XCTestCase {
    func testGainIsOneWhenSlowAndSmoothlyBoundedWhenFast() {
        let a = PointerAcceleration()
        XCTAssertEqual(a.gain(velocity: 50), 1)
        XCTAssertEqual(a.gain(velocity: 120), 1)
        XCTAssertEqual(a.gain(velocity: 100_000), a.maxGain, accuracy: 1e-9)
        var previous: CGFloat = 1
        for v in stride(from: 120.0, through: 1500.0, by: 20.0) {
            let g = a.gain(velocity: CGFloat(v))
            XCTAssertGreaterThanOrEqual(g, previous)
            previous = g
        }
        XCTAssertEqual(a.gain(velocity: .nan), 1)
    }

    func testMomentumDecaysAndStops() {
        var s = MomentumScroller()
        s.pointsPerLine = 10
        _ = s.drag(CGVector(dx: 0, dy: -20), at: 0)
        _ = s.drag(CGVector(dx: 0, dy: -20), at: 0.016)
        _ = s.drag(CGVector(dx: 0, dy: -20), at: 0.032)
        s.lift(at: 0.04)
        XCTAssertTrue(s.coasting)
        var total = 0.0
        var steps = 0
        while let lines = s.coast(interval: 1.0 / 60) {
            XCTAssertGreaterThanOrEqual(lines.dy, 0)
            total += lines.dy
            steps += 1
            if steps > 1000 { break }
        }
        XCTAssertFalse(s.coasting)
        XCTAssertGreaterThan(total, 1)
        XCTAssertLessThan(steps, 400)
        // Planted negative: a slow lift does not coast.
        var slow = MomentumScroller()
        _ = slow.drag(CGVector(dx: 0, dy: -1), at: 0)
        _ = slow.drag(CGVector(dx: 0, dy: -1), at: 0.05)
        slow.lift(at: 0.06)
        XCTAssertFalse(slow.coasting)
    }
}
