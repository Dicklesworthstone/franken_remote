/// USB HID keyboard usages (page 7) the app sends. UIKit's
/// `UIKeyboardHIDUsage` raw values are the same numbers.
enum HIDUsage {
    static let a: UInt16 = 0x04
    static let one: UInt16 = 0x1E
    static let zero: UInt16 = 0x27
    static let returnKey: UInt16 = 0x28
    static let escape: UInt16 = 0x29
    static let backspace: UInt16 = 0x2A
    static let tab: UInt16 = 0x2B
    static let space: UInt16 = 0x2C
    static let f1: UInt16 = 0x3A
    static let home: UInt16 = 0x4A
    static let pageUp: UInt16 = 0x4B
    static let deleteForward: UInt16 = 0x4C
    static let end: UInt16 = 0x4D
    static let pageDown: UInt16 = 0x4E
    static let right: UInt16 = 0x4F
    static let left: UInt16 = 0x50
    static let down: UInt16 = 0x51
    static let up: UInt16 = 0x52
    static let leftControl: UInt16 = 0xE0
    static let leftShift: UInt16 = 0xE1
    static let leftAlt: UInt16 = 0xE2
    static let leftGUI: UInt16 = 0xE3

    /// What frd accepts: keyboard usages 0x04...0xA4 and the eight modifiers.
    static func isSendable(_ usage: UInt16) -> Bool {
        (0x04...0xA4).contains(usage) || (0xE0...0xE7).contains(usage)
    }

    static func function(_ n: Int) -> UInt16? {
        (1...12).contains(n) ? f1 + UInt16(n - 1) : nil
    }
}

/// A software-keyboard character as a US-layout key press. The host turns
/// key presses into characters with ITS layout, so this is exact for hosts
/// using a US layout; anything outside printable ASCII is never guessed and
/// goes to the text path (or is refused) instead.
struct KeyStroke: Equatable {
    var usage: UInt16
    var shift: Bool
}

enum KeyboardMap {
    private static let shiftedDigits: [Character: UInt16] = [
        "!": 0x1E, "@": 0x1F, "#": 0x20, "$": 0x21, "%": 0x22,
        "^": 0x23, "&": 0x24, "*": 0x25, "(": 0x26, ")": 0x27,
    ]
    private static let punctuation: [Character: KeyStroke] = [
        "-": KeyStroke(usage: 0x2D, shift: false), "_": KeyStroke(usage: 0x2D, shift: true),
        "=": KeyStroke(usage: 0x2E, shift: false), "+": KeyStroke(usage: 0x2E, shift: true),
        "[": KeyStroke(usage: 0x2F, shift: false), "{": KeyStroke(usage: 0x2F, shift: true),
        "]": KeyStroke(usage: 0x30, shift: false), "}": KeyStroke(usage: 0x30, shift: true),
        "\\": KeyStroke(usage: 0x31, shift: false), "|": KeyStroke(usage: 0x31, shift: true),
        ";": KeyStroke(usage: 0x33, shift: false), ":": KeyStroke(usage: 0x33, shift: true),
        "'": KeyStroke(usage: 0x34, shift: false), "\"": KeyStroke(usage: 0x34, shift: true),
        "`": KeyStroke(usage: 0x35, shift: false), "~": KeyStroke(usage: 0x35, shift: true),
        ",": KeyStroke(usage: 0x36, shift: false), "<": KeyStroke(usage: 0x36, shift: true),
        ".": KeyStroke(usage: 0x37, shift: false), ">": KeyStroke(usage: 0x37, shift: true),
        "/": KeyStroke(usage: 0x38, shift: false), "?": KeyStroke(usage: 0x38, shift: true),
        " ": KeyStroke(usage: HIDUsage.space, shift: false),
        "\n": KeyStroke(usage: HIDUsage.returnKey, shift: false),
        "\r": KeyStroke(usage: HIDUsage.returnKey, shift: false),
        "\t": KeyStroke(usage: HIDUsage.tab, shift: false),
    ]

    /// The US-layout key for `character`, or nil when there is none.
    static func stroke(for character: Character) -> KeyStroke? {
        guard character.isASCII, let scalar = character.unicodeScalars.first,
              character.unicodeScalars.count == 1 else { return nil }
        let value = scalar.value
        switch value {
        case 0x61...0x7A: // a-z
            return KeyStroke(usage: HIDUsage.a + UInt16(value - 0x61), shift: false)
        case 0x41...0x5A: // A-Z
            return KeyStroke(usage: HIDUsage.a + UInt16(value - 0x41), shift: true)
        case 0x31...0x39: // 1-9
            return KeyStroke(usage: HIDUsage.one + UInt16(value - 0x31), shift: false)
        case 0x30:
            return KeyStroke(usage: HIDUsage.zero, shift: false)
        default:
            if let usage = shiftedDigits[character] { return KeyStroke(usage: usage, shift: true) }
            return punctuation[character]
        }
    }
}

/// The extra-keys bar's sticky modifiers. A tap latches a modifier for the
/// next key; a second tap locks it; a third releases it.
struct StickyModifiers: Equatable {
    enum Modifier: CaseIterable, Hashable {
        case control, option, command, shift
        var usage: UInt16 {
            switch self {
            case .control: return HIDUsage.leftControl
            case .option: return HIDUsage.leftAlt
            case .command: return HIDUsage.leftGUI
            case .shift: return HIDUsage.leftShift
            }
        }
        var title: String {
            switch self {
            case .control: return "ctrl"
            case .option: return "opt"
            case .command: return "cmd"
            case .shift: return "shift"
            }
        }
    }
    enum Level: Equatable { case off, latched, locked }

    private(set) var levels: [Modifier: Level] = [:]

    func level(_ modifier: Modifier) -> Level { levels[modifier] ?? .off }

    mutating func tap(_ modifier: Modifier) {
        switch level(modifier) {
        case .off: levels[modifier] = .latched
        case .latched: levels[modifier] = .locked
        case .locked: levels[modifier] = nil
        }
    }

    /// Active modifiers in a fixed press order.
    var active: [Modifier] { Modifier.allCases.filter { level($0) != .off } }

    /// A key was typed: latched modifiers are used up, locked ones stay.
    mutating func consume() {
        for (modifier, level) in levels where level == .latched { levels[modifier] = nil }
    }

    mutating func clear() { levels.removeAll() }
}

/// Opaque custody of one original callback. Rust owns its source, view,
/// deadline, consumed indices and whole-stroke sequencing. The UI cannot
/// renew the token or transfer it to another session while waiting.
final class TypingCapture {
    private weak var owner: InputSink?
    private let token: UInt64

    private init(owner: InputSink, token: UInt64) {
        self.owner = owner
        self.token = token
    }

    static func capture(into sink: InputSink) -> Result<TypingCapture, InputStatus> {
        sink.beginTyping().flatMap { token in
            token == 0 ? .failure(.invalid) : .success(TypingCapture(owner: sink, token: token))
        }
    }

    func stroke(_ usage: UInt16, modifiers: [UInt16], index: UInt32,
                into sink: InputSink) -> InputStatus {
        guard let owner, owner === sink else { return .stale }
        var mask: UInt8 = 0
        for modifier in modifiers {
            guard (0xE0...0xE7).contains(modifier) else { return .invalid }
            mask |= UInt8(1 << (modifier - 0xE0))
        }
        return owner.typeCaptured(token, index: index, usage: usage, modifiers: mask)
    }

    deinit { owner?.endTyping(token) }
}


/// Completed text callbacks use one immediate attempt. There is no retained
/// text, delayed retry, layout fallback or partial-commit prefix in this policy.
enum UnicodeCommit {
    /// Same complete UTF-8 bound as fr_core::input::MAX_COMMITTED_TEXT_BYTES.
    static let maxBytes = 4096

    static func needsText(for text: String) -> Bool {
        text.contains { KeyboardMap.stroke(for: $0) == nil }
    }

    /// A paste/dictation callback can carry many ASCII characters. When the
    /// host grants whole text and no shortcut modifier is active, send it
    /// once instead of stretching physical keys across an input deadline.
    /// Refusal never falls back to keys. Single ASCII keys keep their mapping.
    static func usesText(for text: String, supported: Bool, modifiersActive: Bool) -> Bool {
        needsText(for: text) || (supported && !modifiersActive && text.count > 1)
    }

    static func valid(_ text: String) -> Bool {
        guard !text.isEmpty,
              text.utf8.prefix(maxBytes + 1).count <= maxBytes else { return false }
        // The existing C ABI takes a C string; NUL would silently truncate it.
        return !text.utf8.contains(0)
    }

    /// Preserve the order of already queued ASCII keys and the meaning of
    /// sticky/hardware modifiers. A refusal consumes none of those states.
    static func submit(_ text: String, pendingKeys: Bool, heldKeys: Bool,
                       modifiersActive: Bool, into sink: InputSink) -> InputStatus {
        guard valid(text) else { return .invalid }
        guard !pendingKeys, !heldKeys, !modifiersActive else { return .busy }
        return sink.text(text)
    }

    static func notice(for status: InputStatus) -> String? {
        switch status {
        case .sent, .pending:
            return nil
        case .busy:
            return "Text was not sent while input is busy or keys are held. Release modifiers and type it again when the view is ready."
        case .unsupported:
            return "This host did not enable Unicode text input. The text was not sent."
        case .invalid:
            return "That text is empty, too large, or contains an unsupported character. Nothing was sent."
        case .notControlled, .controlLost, .closed, .stale:
            return "Text was not sent because control is unavailable."
        case .outside:
            return "Text could not be accepted in the current view. Nothing was sent."
        }
    }
}

/// ASCII strokes waiting under their original Rust capture. Busy retries never
/// recapture, and any expired/refused stroke drops the remaining dependent
/// input. Unicode commits cannot enter this queue. Private, bounded contents.
struct TypingQueue {
    enum Item: Equatable {
        case stroke(UInt16, modifiers: [UInt16])
    }
    /// Most items waiting at once; more are refused.
    static let capacity = 4096

    private struct Pending {
        let item: Item
        let capture: TypingCapture
        let index: UInt32
    }
    private var items: [Pending] = []
    private var head = 0

    var isEmpty: Bool { head >= items.count }
    var count: Int { items.count - head }

    /// A callback is either retained whole or refused before any prefix goes.
    mutating func append(_ new: [Item], capture: TypingCapture) -> Bool {
        compact()
        let room = max(0, Self.capacity - count)
        guard !new.isEmpty, new.count <= room else { return false }
        items.append(contentsOf: new.enumerated().map { index, item in
            Pending(item: item, capture: capture, index: UInt32(index))
        })
        return true
    }

    mutating func clear() {
        items.removeAll()
        head = 0
    }

    struct Outcome: Equatable {
        var sent = 0
        /// Stopped at a busy host; the rest waits.
        var busy = false
        /// Content-free refusal. Earlier admitted strokes remain effects;
        /// the unsubmitted suffix was discarded and is never retried.
        var refusal: InputStatus?
    }

    /// Send in order until the host is busy or everything went. Control
    /// gone drops the rest.
    mutating func drain(into sink: InputSink) -> Outcome {
        var outcome = Outcome()
        while head < items.count {
            let pending = items[head]
            let status: InputStatus
            switch pending.item {
            case .stroke(let usage, let modifiers):
                status = pending.capture.stroke(usage, modifiers: modifiers,
                                                index: pending.index, into: sink)
            }
            switch status {
            case .busy:
                outcome.busy = true
                compact()
                return outcome
            case .sent, .pending:
                outcome.sent += 1
                head += 1
            case .unsupported, .outside, .invalid, .notControlled, .controlLost, .closed, .stale:
                outcome.refusal = status
                clear()
                return outcome
            }
        }
        clear()
        return outcome
    }

    private mutating func compact() {
        if head > 0 {
            items.removeFirst(head)
            head = 0
        }
    }
}
