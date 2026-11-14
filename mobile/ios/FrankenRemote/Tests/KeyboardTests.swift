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

    func testTypingNeverReleasesAKeyItDidNotPress() {
        // Shift is already held on a hardware keyboard: the core answers
        // pending. Planted negative (the old typer): releasing it here
        // would let go of the user's own key.
        let sink = RecordingSink()
        sink.keyAnswer = { usage, pressed in usage == HIDUsage.leftShift && pressed ? .pending : .sent }
        XCTAssertEqual(KeyTyper.type(0x04, modifiers: [HIDUsage.leftShift], into: sink), .sent)
        XCTAssertEqual(sink.calls, [.key(HIDUsage.leftShift, true), .key(0x04, true), .key(0x04, false)])
        // A busy host: nothing typed, every press it made released.
        let busy = RecordingSink()
        busy.keyAnswer = { usage, pressed in usage == 0x04 && pressed ? .busy : .sent }
        XCTAssertEqual(KeyTyper.type(0x04, modifiers: [HIDUsage.leftGUI], into: busy), .busy)
        XCTAssertEqual(busy.calls, [.key(HIDUsage.leftGUI, true), .key(0x04, true), .key(HIDUsage.leftGUI, false)])
    }

    func testQueuedTypingWaitsForABusyHostAndKeepsItsOrder() {
        var queue = TypingQueue()
        let sink = RecordingSink()
        var allow = 2
        // The host takes two presses, then is busy until allowed again.
        sink.keyAnswer = { _, pressed in
            guard pressed else { return .sent }
            if allow == 0 { return .busy }
            allow -= 1
            return .sent
        }
        XCTAssertTrue(queue.append([.stroke(0x04, modifiers: []), .stroke(0x05, modifiers: []),
                                    .stroke(0x06, modifiers: [])]))
        var outcome = queue.drain(into: sink)
        XCTAssertEqual(outcome.sent, 2)
        XCTAssertTrue(outcome.busy)
        XCTAssertEqual(queue.count, 1, "c waits; planted negative: none dropped")
        allow = 5
        outcome = queue.drain(into: sink)
        XCTAssertEqual(outcome.sent, 1)
        XCTAssertTrue(queue.isEmpty)
        let pressed = sink.calls.compactMap { call -> UInt16? in
            if case .key(let usage, true) = call { return usage } else { return nil }
        }
        XCTAssertEqual(pressed, [0x04, 0x05, 0x06, 0x06], "in order; c retried once")
        // Bounded: an over-long paste keeps its start.
        var long = TypingQueue()
        XCTAssertFalse(long.append(Array(repeating: .stroke(0x04, modifiers: []), count: TypingQueue.capacity + 1)))
        XCTAssertEqual(long.count, TypingQueue.capacity)
        // Control gone drops the rest.
        let lost = RecordingSink()
        lost.keyResult = .controlLost
        _ = long.drain(into: lost)
        XCTAssertTrue(long.isEmpty)
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

    func testUnicodeCallbackIsWholeAndAsciiKeepsItsPhysicalKeyPath() {
        XCTAssertFalse(UnicodeCommit.needsText(for: "ASCII A-z 123\t\r\n"))
        XCTAssertTrue(UnicodeCommit.needsText(for: "ASCII é"))
        XCTAssertTrue(UnicodeCommit.needsText(for: "e\u{301}🙂"))
        let sink = RecordingSink()
        sink.textResult = .sent
        let value = "ASCII e\u{301} é 漢🙂\r\n"
        XCTAssertEqual(UnicodeCommit.submit(value, pendingKeys: false, heldKeys: false,
                                            modifiersActive: false, into: sink), .sent)
        XCTAssertEqual(sink.calls, [.text(value)], "one callback, no ASCII prefix or key synthesis")
        guard case .text(let captured)? = sink.calls.first else { return XCTFail("one text call") }
        XCTAssertEqual(Array(captured.utf8), Array(value.utf8), "no normalization or splitting")
        XCTAssertNil(UnicodeCommit.notice(for: .sent))
    }

    func testUnicodeCannotOvertakePendingAsciiOrConsumeModifiers() {
        var queue = TypingQueue()
        XCTAssertTrue(queue.append([.stroke(HIDUsage.a, modifiers: [])]))
        let sink = RecordingSink()
        sink.textResult = .sent
        XCTAssertEqual(UnicodeCommit.submit("é", pendingKeys: !queue.isEmpty, heldKeys: false,
                                            modifiersActive: false, into: sink), .busy)
        XCTAssertTrue(sink.calls.isEmpty, "refuse before draining a prefix")
        XCTAssertEqual(queue.count, 1)
        _ = queue.drain(into: sink)
        XCTAssertEqual(sink.calls, [.key(HIDUsage.a, true), .key(HIDUsage.a, false)])
        // Finishing older keys never retries the refused Unicode callback.
        for (held, modifier) in [(true, false), (false, true), (true, true)] {
            let blocked = RecordingSink()
            XCTAssertEqual(UnicodeCommit.submit("é", pendingKeys: false, heldKeys: held,
                                                modifiersActive: modifier, into: blocked), .busy)
            XCTAssertTrue(blocked.calls.isEmpty)
        }
    }

    func testBusyAndUnsupportedUnicodeAreAttemptedOnceWithoutKeyFallback() {
        for status: InputStatus in [.busy, .unsupported, .notControlled, .controlLost, .closed, .stale] {
            let sink = RecordingSink()
            sink.textResult = status
            let value = "prefix é suffix"
            XCTAssertEqual(UnicodeCommit.submit(value, pendingKeys: false, heldKeys: false,
                                                modifiersActive: false, into: sink), status)
            XCTAssertEqual(sink.calls, [.text(value)])
            XCTAssertNotNil(UnicodeCommit.notice(for: status))
            // A later refresh only drains physical keys. The refused commit
            // is not retained anywhere for a new context or timestamp.
            var queue = TypingQueue()
            sink.textResult = .sent
            _ = queue.drain(into: sink)
            XCTAssertEqual(sink.calls, [.text(value)])
        }
    }

    func testUnicodeByteBoundAndNulRefuseWholeBeforeTheCStringBoundary() {
        let exact = String(repeating: "🙂", count: UnicodeCommit.maxBytes / 4)
        XCTAssertEqual(exact.utf8.count, UnicodeCommit.maxBytes)
        let accepted = RecordingSink()
        accepted.textResult = .sent
        XCTAssertEqual(UnicodeCommit.submit(exact, pendingKeys: false, heldKeys: false,
                                            modifiersActive: false, into: accepted), .sent)
        XCTAssertEqual(accepted.calls, [.text(exact)])
        for value in ["", "before\u{0}after", exact + "a"] {
            let refused = RecordingSink()
            XCTAssertEqual(UnicodeCommit.submit(value, pendingKeys: false, heldKeys: false,
                                                modifiersActive: false, into: refused), .invalid)
            XCTAssertTrue(refused.calls.isEmpty, "no truncated prefix")
        }
        XCTAssertNotNil(UnicodeCommit.notice(for: .invalid))
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
