import CoreGraphics
import CoreVideo
import Foundation

/// Where a session is (FR_STATE_* plus its end).
enum SessionPhase: Equatable {
    case connecting(attempt: Int)
    case authenticated
    case awaitingApproval
    case viewing
    case requestingControl
    case controlled
    case reconnecting(attempt: Int)
    case closing
    case ended(SessionEnd)

    var isEnded: Bool { if case .ended = self { return true } else { return false } }
    var isViewing: Bool {
        switch self {
        case .viewing, .requestingControl, .controlled: return true
        default: return false
        }
    }
}

/// Why a session ended (FR_END_*), worded for the user.
enum SessionEnd: Equatable {
    case closedByYou, hostClosed, nameUnresolved, outsideTailnet, identityChanged, tlsRefused
    case unreachable, hostRefused, hostBusy, sessionFailed, cancelled, internalError
    /// This device stopped control (the view left the screen, input was
    /// refused, or the host's picture changed size); the session ended
    /// with it, as the host requires.
    case controlStopped

    init(code: UInt32) {
        switch Int32(bitPattern: code) {
        case FR_END_OK: self = .closedByYou
        case FR_END_HOST_CLOSED: self = .hostClosed
        case FR_END_NAME_UNRESOLVED: self = .nameUnresolved
        case FR_END_OUTSIDE_TAILNET: self = .outsideTailnet
        case FR_END_IDENTITY_CHANGED: self = .identityChanged
        case FR_END_TLS_REFUSED: self = .tlsRefused
        case FR_END_UNREACHABLE: self = .unreachable
        case FR_END_HOST_REFUSED: self = .hostRefused
        case FR_END_HOST_BUSY: self = .hostBusy
        case FR_END_SESSION_FAILED: self = .sessionFailed
        case FR_END_CANCELLED: self = .cancelled
        case FR_END_CONTROL_STOPPED: self = .controlStopped
        default: self = .internalError
        }
    }

    var title: String {
        switch self {
        case .closedByYou: return "Disconnected"
        case .hostClosed: return "The host ended the session"
        case .nameUnresolved: return "Machine not found"
        case .outsideTailnet: return "Not a tailnet address"
        case .identityChanged: return "This machine's address changed"
        case .tlsRefused: return "Secure connection refused"
        case .unreachable: return "Host unreachable"
        case .hostRefused: return "The host refused the session"
        case .hostBusy: return "The host is busy"
        case .sessionFailed: return "The session failed"
        case .cancelled: return "Connection cancelled"
        case .internalError: return "Something went wrong"
        case .controlStopped: return "Control ended"
        }
    }

    var message: String {
        switch self {
        case .closedByYou: return ""
        case .hostClosed: return "The session closed on the host."
        case .nameUnresolved:
            return "The name did not resolve. Check that Tailscale is connected on this device and that the name matches the machine in the Tailscale app."
        case .outsideTailnet:
            return "The name resolved to an address outside your tailnet, so the app did not connect. Turn on Tailscale and its MagicDNS."
        case .identityChanged:
            return "The name now resolves to different tailnet addresses than when you first connected. If you re-created this machine in Tailscale, forget the saved pin and connect again."
        case .tlsRefused:
            return "The host's certificate was not valid for this name. The host must serve its Tailscale HTTPS certificate."
        case .unreachable:
            return "No answer from the host. Check that FrankenRemote is running there and that both devices are online in Tailscale."
        case .hostRefused:
            return "The host's policy refused this session, or the host declined approval."
        case .hostBusy:
            return "Someone else is viewing this machine, and it serves one viewer at a time. Try again after that session ends."
        case .sessionFailed: return "The session ended unexpectedly."
        case .cancelled: return "The connection was cancelled."
        case .internalError: return "The app could not run the session."
        case .controlStopped:
            return "Control stops the moment FrankenRemote leaves the screen (Control Center, a notification, another app) or the host's screen changes size, and the session ends with it. Try again to take control again."
        }
    }

    var canRetry: Bool {
        switch self {
        case .closedByYou, .outsideTailnet, .identityChanged, .tlsRefused: return false
        default: return true
        }
    }
}

/// One live session over the C ABI. Main thread: the app polls it once per
/// display refresh (`pump`). Input calls are cheap and never block.
final class RemoteSession: ObservableObject, InputSink {
    let host: SavedHost
    let handle: FrHandle
    let controlCapable: Bool

    @Published private(set) var phase: SessionPhase = .connecting(attempt: 1)
    /// Encoded picture size (pixels); zero until the first configuration.
    @Published private(set) var picture: CGSize = .zero
    /// The host display's size (pixels).
    @Published private(set) var display: CGSize = .zero
    /// A short message for the user (e.g. a character the host can't take).
    @Published var notice: String?
    @Published private(set) var detail: String = ""
    /// The pin this session observed (for saving with the host).
    @Published private(set) var observedPin: String?

    private var wantsControl: Bool
    private(set) var everViewed = false

    enum StartError: Error, Equatable {
        case invalid, busy, other(Int32)
    }

    /// `screen`: this device's pixels, longer side first (the largest picture
    /// it shows 1:1), or 0, 0 for the host's native pixels.
    init(host: SavedHost, control: Bool, requestControl: Bool,
         screen: (width: UInt32, height: UInt32) = (0, 0)) throws {
        self.host = host
        self.controlCapable = control
        self.wantsControl = control && requestControl
        var out: FrHandle = 0
        let status: Int32 = host.name.withCString { name in
            func start(_ pin: UnsafePointer<CChar>?) -> Int32 {
                var config = FrConfig(host: name, port: host.port, control: control,
                                      direct: host.dialsDirectly, pin: pin,
                                      picture_width: screen.width,
                                      picture_height: screen.height)
                return fr_session_start(&config, &out)
            }
            if let pin = host.pin {
                return pin.withCString { start($0) }
            }
            return start(nil)
        }
        switch status {
        case FR_OK: handle = out
        case FR_INVALID: throw StartError.invalid
        case FR_BUSY: throw StartError.busy
        default: throw StartError.other(status)
        }
        if wantsControl {
            _ = fr_session_request_control(handle)
        }
    }

    deinit {
        _ = fr_session_free(handle)
    }

    /// Drain pending events. Call on the main thread, once per refresh.
    func pump() {
        var event = FrEvent()
        while fr_session_poll_event(handle, &event) {
            apply(event)
        }
    }

    private func apply(_ event: FrEvent) {
        switch Int32(bitPattern: event.kind) {
        case FR_EVENT_STATE:
            let attempt = Int(event.attempt)
            switch Int32(bitPattern: event.value) {
            case FR_STATE_CONNECTING: setPhase(.connecting(attempt: max(1, attempt)))
            case FR_STATE_AUTHENTICATED: setPhase(.authenticated)
            case FR_STATE_AWAITING_APPROVAL: setPhase(.awaitingApproval)
            case FR_STATE_VIEWING:
                everViewed = true
                setPhase(.viewing)
            case FR_STATE_REQUESTING_CONTROL: setPhase(.requestingControl)
            case FR_STATE_CONTROLLED: setPhase(.controlled)
            case FR_STATE_RECONNECTING: setPhase(.reconnecting(attempt: attempt))
            case FR_STATE_CLOSING: setPhase(.closing)
            case FR_STATE_CONTROL_ENDED:
                if phase == .controlled || phase == .requestingControl { setPhase(.closing) }
            default: break
            }
        case FR_EVENT_PIN:
            let pin = copyString(fr_session_copy_pin)
            if !pin.isEmpty, pin != observedPin { observedPin = pin }
        case FR_EVENT_VIDEO:
            if event.picture_width > 0, event.picture_height > 0 {
                let size = CGSize(width: Int(event.picture_width), height: Int(event.picture_height))
                if size != picture { picture = size }
            }
            if event.display_width > 0, event.display_height > 0 {
                let size = CGSize(width: Int(event.display_width), height: Int(event.display_height))
                if size != display { display = size }
            }
        case FR_EVENT_ENDED:
            detail = copyString(fr_session_copy_detail)
            setPhase(.ended(SessionEnd(code: event.value)))
        case FR_EVENT_OVERFLOW:
            // Events were dropped; the next state event corrects the phase.
            break
        default:
            break
        }
    }

    private func setPhase(_ next: SessionPhase) {
        if phase.isEnded { return }
        if phase != next { phase = next }
    }

    private func copyString(_ copy: (FrHandle, UnsafeMutablePointer<CChar>?, Int) -> Int) -> String {
        var capacity = 256
        var buffer = [CChar](repeating: 0, count: capacity)
        let length = copy(handle, &buffer, capacity)
        if length >= capacity {
            capacity = length + 1
            buffer = [CChar](repeating: 0, count: capacity)
            _ = copy(handle, &buffer, capacity)
        }
        return String(decoding: buffer.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
    }

    // MARK: Video

    struct Frame {
        let buffer: CVPixelBuffer
        let info: FrFrameInfo
    }

    /// The newest picture after `sequence`, if any.
    func frame(after sequence: UInt64) -> Frame? {
        var info = FrFrameInfo()
        guard let buffer = fr_session_copy_frame(handle, sequence, &info) else { return nil }
        return Frame(buffer: buffer, info: info)
    }

    /// The picture with frame id `frame` reached the screen.
    func drawn(_ frame: UInt64) {
        fr_session_frame_drawn(handle, frame)
    }

    func setVisible(_ visible: Bool) {
        fr_session_set_visible(handle, visible)
    }

    func stats() -> FrStats {
        var stats = FrStats()
        _ = fr_session_stats(handle, &stats)
        return stats
    }

    struct CursorShape {
        var state: FrCursor
        var rgba: [UInt8]
    }

    /// The host-drawn cursor when the host sends it separately.
    func cursor(knownGeneration: UInt64) -> CursorShape? {
        var state = FrCursor()
        guard fr_session_cursor(handle, &state), state.generation != knownGeneration else { return nil }
        let length = fr_session_copy_cursor_rgba(handle, nil, 0)
        var rgba = [UInt8](repeating: 0, count: length)
        if length > 0 {
            _ = fr_session_copy_cursor_rgba(handle, &rgba, rgba.count)
        }
        return CursorShape(state: state, rgba: rgba)
    }

    // MARK: Control

    func requestControl() {
        guard controlCapable else { return }
        wantsControl = true
        _ = fr_session_request_control(handle)
    }

    /// Ending control ends the session (the host's rule); reconnect to view.
    func releaseControl() {
        _ = fr_session_release_all(handle)
        fr_session_release_control(handle)
    }

    func close() {
        _ = fr_session_release_all(handle)
        _ = fr_session_close(handle)
    }

    // MARK: InputSink

    @discardableResult func pointer(to point: CGPoint) -> InputStatus {
        InputStatus(code: fr_session_pointer(handle, Double(point.x), Double(point.y)))
    }

    @discardableResult func button(_ button: PointerButton, pressed: Bool, at point: CGPoint) -> InputStatus {
        InputStatus(code: fr_session_button(handle, button.rawValue, pressed,
                                            Double(point.x), Double(point.y)))
    }

    @discardableResult func scroll(dx: Double, dy: Double, at point: CGPoint) -> InputStatus {
        InputStatus(code: fr_session_scroll(handle, dx, dy, Double(point.x), Double(point.y)))
    }

    func scrollEnded() {
        fr_session_scroll_end(handle)
    }

    @discardableResult func key(_ usage: UInt16, pressed: Bool) -> InputStatus {
        InputStatus(code: fr_session_key(handle, usage, pressed))
    }

    @discardableResult func text(_ text: String) -> InputStatus {
        InputStatus(code: text.withCString { fr_session_text(handle, $0) })
    }

    @discardableResult func releaseAllKeys() -> InputStatus {
        InputStatus(code: fr_session_release_all(handle))
    }
}
