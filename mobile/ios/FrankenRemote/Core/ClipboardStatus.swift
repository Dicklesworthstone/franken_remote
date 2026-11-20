/// Content-free evidence for one original native clipboard owner. Native
/// startup and OS publication counts do not imply an active input grant.
struct ClipboardStatus: Equatable {
    var requested = false
    var enabled = false
    var started = false
    var reason: UInt32 = 0
    var received: UInt64 = 0
    var uncertain: UInt64 = 0
    var refused: UInt64 = 0

    init(requested: Bool = false) {
        self.requested = requested
        enabled = requested
    }

    init(_ native: FrClipboardStatus) {
        requested = native.requested == 1
        enabled = requested && native.enabled == 1
        started = native.started == 1
        reason = native.reason
        received = native.received_submitted
        uncertain = native.received_unknown
        refused = native.received_refused
    }

    func title(controlled: Bool, ended: Bool) -> String {
        if !requested { return "Clipboard off" }
        if reason == 1 { return "Clipboard unavailable on this host" }
        if reason == 3 { return "Clipboard sharing ended" }
        if !enabled || ended { return "Clipboard off" }
        if !controlled { return "Clipboard on after control is granted" }
        return started ? "Text clipboard sharing on" : "Opening text clipboard…"
    }
}
