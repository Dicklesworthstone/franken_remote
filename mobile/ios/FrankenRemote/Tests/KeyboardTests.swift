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

    func testTypingHandsOneWholeStrokeToRustWithoutIndependentModifierEffects() throws {
        var queue = TypingQueue()
        let sink = RecordingSink()
        XCTAssertTrue(queue.append([.stroke(4, modifiers: [HIDUsage.leftShift, HIDUsage.leftGUI,
                                                           HIDUsage.leftShift])],
                                   capture: try TypingCapture.capture(into: sink).get()))
        XCTAssertEqual(queue.drain(into: sink).sent, 1)
        XCTAssertEqual(sink.calls, [.stroke(1, 0, 4, 0b1010)])
        XCTAssertEqual(sink.capturesCreated, 1)
        XCTAssertEqual(sink.capturesEnded, [1])
    }

    func testQueuedTypingWaitsUnderItsOriginalCaptureAndKeepsItsIndices() throws {
        var queue = TypingQueue()
        let sink = RecordingSink()
        var allow = 2
        sink.strokeAnswer = { _, _, _, _ in
            if allow == 0 { return .busy }
            allow -= 1
            return .sent
        }
        XCTAssertTrue(queue.append([.stroke(0x04, modifiers: []), .stroke(0x05, modifiers: []),
                                    .stroke(0x06, modifiers: [])],
                                   capture: try TypingCapture.capture(into: sink).get()))
        var outcome = queue.drain(into: sink)
        XCTAssertEqual(outcome.sent, 2)
        XCTAssertTrue(outcome.busy)
        XCTAssertEqual(queue.count, 1, "c waits; planted negative: none dropped")
        allow = 5
        outcome = queue.drain(into: sink)
        XCTAssertEqual(outcome.sent, 1)
        XCTAssertTrue(queue.isEmpty)
        XCTAssertEqual(sink.calls, [.stroke(1, 0, 4, 0), .stroke(1, 1, 5, 0),
                                    .stroke(1, 2, 6, 0), .stroke(1, 2, 6, 0)])
        XCTAssertEqual(sink.capturesCreated, 1, "busy never recaptures a callback")
    }

    func testOversizedCallbackIsRefusedWholeWithoutAQueuedPrefix() throws {
        var long = TypingQueue()
        let sink = RecordingSink()
        XCTAssertFalse(long.append(Array(repeating: .stroke(4, modifiers: []), count: TypingQueue.capacity + 1),
                                    capture: try TypingCapture.capture(into: sink).get()))
        XCTAssertTrue(long.isEmpty)
        XCTAssertTrue(sink.calls.isEmpty)
        XCTAssertEqual(sink.capturesEnded, [1])
        XCTAssertTrue(long.append([.stroke(4, modifiers: [])], capture: try TypingCapture.capture(into: sink).get()))
        XCTAssertFalse(long.append(Array(repeating: .stroke(5, modifiers: []), count: TypingQueue.capacity),
                                    capture: try TypingCapture.capture(into: sink).get()))
        XCTAssertEqual(long.count, 1)
        XCTAssertEqual(long.drain(into: sink).sent, 1)
        XCTAssertEqual(sink.calls, [.stroke(2, 0, 4, 0)])
    }

    func testExpiredOriginalTypingDropsTheSuffixWithoutRecapture() throws {
        var queue = TypingQueue()
        let sink = RecordingSink()
        sink.strokeResult = .busy
        XCTAssertTrue(queue.append([.stroke(4, modifiers: [HIDUsage.leftGUI]), .stroke(5, modifiers: [])],
                                   capture: try TypingCapture.capture(into: sink).get()))
        XCTAssertTrue(queue.drain(into: sink).busy)
        // Rust's actual exclusive deadline/context enforcement has separate
        // source tests. The shell must obey this refusal without recapturing.
        sink.strokeResult = .controlLost
        XCTAssertEqual(queue.drain(into: sink).refusal, .controlLost)
        XCTAssertTrue(queue.isEmpty)
        sink.strokeResult = .sent
        XCTAssertEqual(queue.drain(into: sink).sent, 0)
        XCTAssertEqual(sink.calls, [.stroke(1, 0, 4, 8), .stroke(1, 0, 4, 8)])
        XCTAssertEqual(sink.capturesCreated, 1)
        XCTAssertEqual(sink.capturesEnded, [1])
    }

    func testReplacementSessionCannotUseAnOldCaptureAndClearingRetiresIt() throws {
        var queue = TypingQueue()
        let original = RecordingSink()
        let replacement = RecordingSink()
        XCTAssertTrue(queue.append([.stroke(4, modifiers: [])],
                                   capture: try TypingCapture.capture(into: original).get()))
        XCTAssertEqual(queue.drain(into: replacement).refusal, .stale)
        XCTAssertTrue(original.calls.isEmpty)
        XCTAssertTrue(replacement.calls.isEmpty)
        XCTAssertEqual(original.capturesEnded, [1])
        XCTAssertEqual(replacement.capturesCreated, 0)
        XCTAssertTrue(queue.append([.stroke(5, modifiers: [])],
                                   capture: try TypingCapture.capture(into: original).get()))
        queue.clear()
        XCTAssertEqual(original.capturesEnded, [1, 2])
        XCTAssertTrue(queue.isEmpty)
    }

    func testRefusedCaptureDoesNotCreateRetryableTyping() {
        let sink = RecordingSink()
        sink.captureRefusal = .busy
        if case .success = TypingCapture.capture(into: sink) { XCTFail("must refuse") }
        XCTAssertEqual(sink.capturesCreated, 0)
        XCTAssertTrue(sink.calls.isEmpty)
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

    func testNegotiatedAsciiPasteUsesWholeTextOnceAndKeepsShortcutSemantics() {
        XCTAssertTrue(UnicodeCommit.usesText(for: "paste once", supported: true, modifiersActive: false))
        XCTAssertFalse(UnicodeCommit.usesText(for: "paste once", supported: false, modifiersActive: false))
        XCTAssertFalse(UnicodeCommit.usesText(for: "ab", supported: true, modifiersActive: true))
        XCTAssertFalse(UnicodeCommit.usesText(for: "a", supported: true, modifiersActive: false))
        XCTAssertTrue(UnicodeCommit.usesText(for: "é", supported: false, modifiersActive: true))
        for status: InputStatus in [.sent, .busy, .unsupported, .controlLost] {
            let sink = RecordingSink()
            sink.textResult = status
            XCTAssertEqual(UnicodeCommit.submit("paste once", pendingKeys: false, heldKeys: false,
                                                modifiersActive: false, into: sink), status)
            XCTAssertEqual(sink.calls, [.text("paste once")])
            XCTAssertEqual(sink.capturesCreated, 0, "no physical-key fallback or retry")
        }
    }

    func testUnicodeCannotOvertakePendingAsciiOrConsumeModifiers() throws {
        var queue = TypingQueue()
        let sink = RecordingSink()
        XCTAssertTrue(queue.append([.stroke(HIDUsage.a, modifiers: [])],
                                   capture: try TypingCapture.capture(into: sink).get()))
        sink.textResult = .sent
        XCTAssertEqual(UnicodeCommit.submit("é", pendingKeys: !queue.isEmpty, heldKeys: false,
                                            modifiersActive: false, into: sink), .busy)
        XCTAssertTrue(sink.calls.isEmpty, "refuse before draining a prefix")
        XCTAssertEqual(queue.count, 1)
        _ = queue.drain(into: sink)
        XCTAssertEqual(sink.calls, [.stroke(1, 0, HIDUsage.a, 0)])
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
