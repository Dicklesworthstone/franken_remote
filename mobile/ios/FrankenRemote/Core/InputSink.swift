import CoreGraphics

/// What an input call did (`FR_*` status codes in fr_mobile.h).
enum InputStatus: Equatable {
    case sent
    /// Accepted with nothing sent yet (a sub-line scroll, an unchanged key).
    case pending
    case notControlled
    /// The position is outside the picture; nothing was sent.
    case outside
    /// The host did not grant this kind of input.
    case unsupported
    case invalid
    /// Control (and so the session) is ending.
    case controlLost
    case closed
    case stale

    init(code: Int32) {
        switch code {
        case 0: self = .sent
        case 1: self = .pending
        case -2: self = .notControlled
        case -3: self = .outside
        case -4: self = .unsupported
        case -5: self = .controlLost
        case -6: self = .closed
        case -8: self = .stale
        default: self = .invalid
        }
    }
}

enum PointerButton: UInt32 {
    case primary = 0
    case secondary = 1
    case middle = 2
}

/// Where gestures and keys go. The live session implements it over the C
/// ABI; positions are encoded picture pixels.
protocol InputSink: AnyObject {
    @discardableResult func pointer(to point: CGPoint) -> InputStatus
    @discardableResult func button(_ button: PointerButton, pressed: Bool, at point: CGPoint) -> InputStatus
    /// Fractional lines; positive `dy` scrolls toward the end of a document.
    @discardableResult func scroll(dx: Double, dy: Double, at point: CGPoint) -> InputStatus
    func scrollEnded()
    @discardableResult func key(_ usage: UInt16, pressed: Bool) -> InputStatus
    @discardableResult func text(_ text: String) -> InputStatus
}
