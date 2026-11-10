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

/// Types keys through a sink: modifiers down, key down, key up, modifiers up
/// (reverse order). No repeat is ever generated.
struct KeyTyper {
    /// Press-and-release `usage` with `modifiers` held. Returns the first
    /// failure, if any (`.busy`: nothing was typed, send it again later).
    /// Only keys this call pressed are released: a key already down
    /// (`.pending`, e.g. held on a hardware keyboard) stays its owner's.
    @discardableResult
    static func type(_ usage: UInt16, modifiers: [UInt16], into sink: InputSink) -> InputStatus {
        var pressed: [UInt16] = []
        var seen: [UInt16] = []
        var result = InputStatus.sent
        for modifier in modifiers where !seen.contains(modifier) {
            seen.append(modifier)
            let status = sink.key(modifier, pressed: true)
            if status == .sent {
                pressed.append(modifier)
            } else if status != .pending {
                result = status
                break
            }
        }
        if result == .sent {
            let down = sink.key(usage, pressed: true)
            if down == .sent {
                let up = sink.key(usage, pressed: false)
                if up != .sent, up != .pending { result = up }
            } else {
                // `.pending`: already held elsewhere; a release here would
                // let go of the user's own key, so nothing is typed.
                result = down
            }
        }
        for modifier in pressed.reversed() {
            sink.key(modifier, pressed: false)
        }
        return result
    }
}

/// Software-keyboard input waiting for the host, sent in order. The core
/// answers `.busy` while the host's input queue is backed up (a paste
/// arrives faster than the host takes keys): the drain stops there and the
/// rest goes on a later call. Bounded; holds what the user typed, so it is
/// never logged or quoted.
struct TypingQueue {
    enum Item: Equatable {
        case stroke(UInt16, modifiers: [UInt16])
        case text(String)
    }
    /// Most items waiting at once; more are refused.
    static let capacity = 4096

    private var items: [Item] = []
    private var head = 0

    var isEmpty: Bool { head >= items.count }
    var count: Int { items.count - head }

    /// Queue `new` after what is waiting. False when some did not fit
    /// (those are dropped, the rest kept in order).
    mutating func append(_ new: [Item]) -> Bool {
        compact()
        let room = max(0, Self.capacity - count)
        items.append(contentsOf: new.prefix(room))
        return new.count <= room
    }

    mutating func clear() {
        items.removeAll()
        head = 0
    }

    struct Outcome: Equatable {
        var sent = 0
        /// Text the host does not take (key presses only) was dropped.
        var unsupported = false
        /// Stopped at a busy host; the rest waits.
        var busy = false
    }

    /// Send in order until the host is busy or everything went. Control
    /// gone drops the rest.
    mutating func drain(into sink: InputSink) -> Outcome {
        var outcome = Outcome()
        while head < items.count {
            let item = items[head]
            let status: InputStatus
            switch item {
            case .stroke(let usage, let modifiers):
                status = KeyTyper.type(usage, modifiers: modifiers, into: sink)
            case .text(let text):
                status = sink.text(text)
            }
            switch status {
            case .busy:
                outcome.busy = true
                compact()
                return outcome
            case .sent, .pending:
                outcome.sent += 1
                head += 1
            case .unsupported, .outside, .invalid:
                if case .text = item { outcome.unsupported = true }
                head += 1
            case .notControlled, .controlLost, .closed, .stale:
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
