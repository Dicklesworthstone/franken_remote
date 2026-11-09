import CoreGraphics
@testable import FrankenRemote

/// Records what gestures and keys asked the session to send. Unit-test
/// double for the state machines only; it proves nothing about a host.
final class RecordingSink: InputSink {
    enum Call: Equatable {
        case pointer(CGPoint)
        case button(PointerButton, Bool, CGPoint)
        case scroll(Double, Double, CGPoint)
        case scrollEnded
        case key(UInt16, Bool)
        case text(String)
    }
    var calls: [Call] = []
    var keyResult = InputStatus.sent
    var textResult = InputStatus.unsupported

    func pointer(to point: CGPoint) -> InputStatus { calls.append(.pointer(point)); return .sent }
    func button(_ button: PointerButton, pressed: Bool, at point: CGPoint) -> InputStatus {
        calls.append(.button(button, pressed, point)); return .sent
    }
    func scroll(dx: Double, dy: Double, at point: CGPoint) -> InputStatus {
        calls.append(.scroll(dx, dy, point)); return .sent
    }
    func scrollEnded() { calls.append(.scrollEnded) }
    func key(_ usage: UInt16, pressed: Bool) -> InputStatus { calls.append(.key(usage, pressed)); return keyResult }
    func text(_ text: String) -> InputStatus { calls.append(.text(text)); return textResult }

    var buttons: [Call] { calls.filter { if case .button = $0 { return true } else { return false } } }
    var pointers: [CGPoint] {
        calls.compactMap { if case .pointer(let p) = $0 { return p } else { return nil } }
    }
    var scrolls: [(Double, Double)] {
        calls.compactMap { if case .scroll(let x, let y, _) = $0 { return (x, y) } else { return nil } }
    }
}
