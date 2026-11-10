import CoreGraphics

/// How one finger drives the host pointer while in control.
enum TouchMode: String, CaseIterable, Identifiable {
    /// The phone is a trackpad: the finger moves the pointer relatively,
    /// tap clicks, touch-and-hold then drag drags, two fingers scroll.
    case trackpad
    /// The finger is the pointer: tap clicks where touched, drag drags,
    /// touch and hold right-clicks.
    case direct

    var id: String { rawValue }
    var title: String {
        switch self {
        case .trackpad: return "Trackpad"
        case .direct: return "Direct touch"
        }
    }
}

/// What a touch did besides input to the host.
enum TouchEffect: Equatable {
    /// The zoom or pan changed: redraw.
    case viewport
    /// A gesture changed meaning (drag started, right click): tap feedback.
    case haptic
}

/// The touch state machine. It turns raw touches (view points, timestamps in
/// seconds) into host input through `sink`, and zoom/pan into `mapper`.
///
/// Without control, touches only move the view: one finger pans a zoomed
/// picture, two fingers pinch-zoom and pan, double tap toggles zoom. With
/// control, see `TouchMode`; two fingers scroll (natural direction, with
/// momentum) or pinch-zoom the view in both modes, and a two-finger tap is a
/// right click. A touch outside the picture never becomes input at a clamped
/// edge position: it is simply not sent.
final class TouchInterpreter {
    var mode: TouchMode = .trackpad {
        didSet { if mode != oldValue { cancelAll() } }
    }
    /// Whether input goes to the host (granted control).
    var controlling = false {
        didSet { if controlling != oldValue { cancelAll() } }
    }
    var screenScale: CGFloat = 3
    var acceleration = PointerAcceleration()
    var scroller = MomentumScroller()
    var mapper = PictureMapper()
    weak var sink: InputSink?

    // Tuning (points and seconds). A still touch is a tap until the hold
    // (trackpad drag) or long press (direct right click) takes it over: no
    // dead band where a press does nothing. Without control, `holdTime`
    // bounds a tap too.
    var tapSlop: CGFloat = 10
    var doubleTapTime = 0.3
    var holdTime = 0.5
    var longPressTime = 0.5
    var twoFingerTapTime = 0.3
    var pinchThreshold: CGFloat = 24
    var edgeMargin: CGFloat = 44
    var autoPanSpeed: CGFloat = 700
    /// How long after this device last moved the trackpad pointer the cursor
    /// is still drawn where it put it (`localPointer(at:)`), not where the
    /// host last reported its cursor (one network round trip behind).
    var localPointerHold = 1.0

    /// The trackpad pointer, in picture pixels.
    private(set) var pointer: CGPoint?
    /// When this device last moved the trackpad pointer (touch time).
    private var pointerMovedAt: Double?

    private struct One {
        var id: Int
        var start: CGPoint
        var startTime: Double
        var last: CGPoint
        var lastTime: Double
        var moved = false
        /// Direct mode: the picture point first touched (nil outside).
        var startPicture: CGPoint?
    }
    private enum TwoKind { case undecided, scroll, zoom }
    private struct Two {
        var ids: (Int, Int)
        var startTime: Double
        var startCentroid: CGPoint
        var startDistance: CGFloat
        var lastCentroid: CGPoint
        var lastDistance: CGFloat
        var kind = TwoKind.undecided
        /// Either finger left the tap slop: no longer a two-finger tap.
        var moved = false
    }
    private enum State {
        case idle
        case one(One)
        case trackpadDrag(id: Int, last: CGPoint, lastTime: Double)
        case directDrag(id: Int, last: CGPoint, lastInside: CGPoint)
        case two(Two)
        /// Until every finger lifts.
        case ignoring
    }
    private var state = State.idle
    private var touches: [Int: CGPoint] = [:]
    private var lastTick: Double?
    private var lastTap: (time: Double, location: CGPoint)?
    private var scrollAnchor: CGPoint?

    var isIdle: Bool {
        if case .idle = state { return touches.isEmpty && !scroller.coasting }
        return false
    }

    // MARK: Touches

    func began(_ id: Int, at location: CGPoint, time: Double) -> [TouchEffect] {
        var effects: [TouchEffect] = []
        if scroller.coasting {
            scroller.stop()
            sink?.scrollEnded()
        }
        touches[id] = location
        switch state {
        case .idle:
            let picture = mapper.toPicture(location)
            state = .one(One(id: id, start: location, startTime: time, last: location,
                             lastTime: time, startPicture: picture))
            if controlling, mode == .direct, let picture {
                sink?.pointer(to: picture)
            }
        case .one(let one):
            guard let first = touches[one.id] else {
                state = .ignoring
                break
            }
            let distance = Self.distance(first, location)
            let centroid = Self.midpoint(first, location)
            state = .two(Two(ids: (one.id, id), startTime: time, startCentroid: centroid,
                             startDistance: distance, lastCentroid: centroid,
                             lastDistance: distance))
        case .two(let two):
            endTwo(two, time: time, lifted: false, effects: &effects)
            state = .ignoring
        case .trackpadDrag, .directDrag, .ignoring:
            break
        }
        return effects
    }

    func moved(_ id: Int, to location: CGPoint, time: Double) -> [TouchEffect] {
        guard touches[id] != nil else { return [] }
        touches[id] = location
        var effects: [TouchEffect] = []
        switch state {
        case .one(var one) where one.id == id:
            if !one.moved, Self.distance(location, one.start) > tapSlop {
                one.moved = true
            }
            guard one.moved else { break }
            let delta = CGVector(dx: location.x - one.last.x, dy: location.y - one.last.y)
            let interval = time - one.lastTime
            one.last = location
            one.lastTime = time
            if !controlling {
                mapper.pan(by: delta)
                effects.append(.viewport)
                state = .one(one)
            } else if mode == .trackpad {
                movePointer(delta, interval: interval, at: time, effects: &effects)
                state = .one(one)
            } else if let start = one.startPicture {
                sink?.button(.primary, pressed: true, at: start)
                var inside = start
                if let point = mapper.toPicture(location) {
                    sink?.pointer(to: point)
                    inside = point
                }
                state = .directDrag(id: id, last: location, lastInside: inside)
            } else {
                // Began outside the picture: it moves the view, never input.
                mapper.pan(by: delta)
                effects.append(.viewport)
                state = .one(one)
            }
        case .trackpadDrag(let drag, let last, let lastTime) where drag == id:
            let delta = CGVector(dx: location.x - last.x, dy: location.y - last.y)
            movePointer(delta, interval: time - lastTime, at: time, effects: &effects)
            state = .trackpadDrag(id: id, last: location, lastTime: time)
        case .directDrag(let drag, _, var inside) where drag == id:
            if let point = mapper.toPicture(location) {
                sink?.pointer(to: point)
                inside = point
            }
            state = .directDrag(id: id, last: location, lastInside: inside)
        case .two(var two) where two.ids.0 == id || two.ids.1 == id:
            guard let a = touches[two.ids.0], let b = touches[two.ids.1] else { break }
            let centroid = Self.midpoint(a, b)
            let distance = Self.distance(a, b)
            if two.kind == .undecided {
                let travel = Self.distance(centroid, two.startCentroid)
                let spread = abs(distance - two.startDistance)
                if travel > tapSlop || spread > tapSlop { two.moved = true }
                // A pinch with one finger still moves the centroid half as
                // far as it spreads: spread at least the travel is a zoom, and
                // a scroll needs fingers moving together (spread under half
                // the travel). Ambiguous strokes decide once clearly long.
                if spread > pinchThreshold, spread >= travel {
                    two.kind = .zoom
                } else if travel > tapSlop, !controlling || spread < travel / 2 {
                    two.kind = controlling ? .scroll : .zoom
                } else if travel > 3 * tapSlop {
                    two.kind = controlling && spread < travel ? .scroll : .zoom
                }
            }
            let delta = CGVector(dx: centroid.x - two.lastCentroid.x,
                                 dy: centroid.y - two.lastCentroid.y)
            switch two.kind {
            case .undecided:
                break
            case .scroll:
                let lines = scroller.drag(delta, at: time)
                if let anchor = scrollPoint(centroid) {
                    scrollAnchor = anchor
                    sink?.scroll(dx: lines.dx, dy: lines.dy, at: anchor)
                }
            case .zoom:
                if two.lastDistance > 0, distance > 0 {
                    mapper.zoom(by: distance / two.lastDistance, at: centroid)
                }
                mapper.pan(by: delta)
                effects.append(.viewport)
            }
            if two.kind != .undecided {
                two.lastCentroid = centroid
                two.lastDistance = distance
            }
            state = .two(two)
        default:
            break
        }
        return effects
    }

    func ended(_ id: Int, at location: CGPoint, time: Double) -> [TouchEffect] {
        guard touches[id] != nil else { return [] }
        touches[id] = location
        var effects: [TouchEffect] = []
        switch state {
        case .one(let one) where one.id == id:
            if !one.moved, time - one.startTime < tapLimit {
                tap(one, at: location, time: time, effects: &effects)
            }
            state = .idle
        case .trackpadDrag(let drag, _, _) where drag == id:
            sink?.button(.primary, pressed: false, at: ensurePointer())
            state = .idle
        case .directDrag(let drag, _, let inside) where drag == id:
            sink?.button(.primary, pressed: false, at: inside)
            state = .idle
        case .two(let two) where two.ids.0 == id || two.ids.1 == id:
            endTwo(two, time: time, lifted: true, effects: &effects)
            state = .ignoring
        default:
            break
        }
        touches[id] = nil
        if touches.isEmpty, case .ignoring = state { state = .idle }
        return effects
    }

    func cancelled(_ id: Int, time: Double) -> [TouchEffect] {
        guard touches[id] != nil else { return [] }
        touches[id] = nil
        releaseHeld()
        if case .two(let two) = state, two.kind == .scroll {
            scroller.stop()
            sink?.scrollEnded()
        }
        state = touches.isEmpty ? .idle : .ignoring
        return []
    }

    /// Advance timers: hold-to-drag, long-press right click, momentum and
    /// edge auto-pan. Call once per display refresh.
    func tick(time: Double) -> [TouchEffect] {
        let interval = lastTick.map { min(0.1, max(0, time - $0)) } ?? 0
        lastTick = time
        var effects: [TouchEffect] = []
        if scroller.coasting, let lines = scroller.coast(interval: interval) {
            if let anchor = scrollAnchor {
                sink?.scroll(dx: lines.dx, dy: lines.dy, at: anchor)
            }
            if !scroller.coasting { sink?.scrollEnded() }
        }
        switch state {
        case .one(let one) where controlling && !one.moved:
            if mode == .trackpad, time - one.startTime >= holdTime {
                sink?.button(.primary, pressed: true, at: ensurePointer())
                // From where the finger is now (it may have crept inside
                // the slop), so the drag never jumps at its first move.
                state = .trackpadDrag(id: one.id, last: touches[one.id] ?? one.last, lastTime: time)
                effects.append(.haptic)
            } else if mode == .direct, time - one.startTime >= longPressTime,
                      let point = one.startPicture {
                sink?.button(.secondary, pressed: true, at: point)
                sink?.button(.secondary, pressed: false, at: point)
                state = .ignoring
                effects.append(.haptic)
            }
        case .directDrag(let id, let last, var inside) where interval > 0:
            let push = edgePush(last)
            if push != .zero {
                let before = mapper
                mapper.pan(by: CGVector(dx: push.dx * autoPanSpeed * CGFloat(interval),
                                        dy: push.dy * autoPanSpeed * CGFloat(interval)))
                if mapper != before {
                    effects.append(.viewport)
                    if let point = mapper.toPicture(last) {
                        sink?.pointer(to: point)
                        inside = point
                    }
                    state = .directDrag(id: id, last: last, lastInside: inside)
                }
            }
        default:
            break
        }
        return effects
    }

    /// Stop every gesture now: release a held button, end scrolling. Used
    /// when control or the mode changes, the view hides, or touches cancel.
    func cancelAll() {
        releaseHeld()
        if scroller.coasting { sink?.scrollEnded() }
        if case .two(let two) = state, two.kind == .scroll { sink?.scrollEnded() }
        scroller.stop()
        state = touches.isEmpty ? .idle : .ignoring
    }

    /// Forget every touch and the trackpad pointer (a new session, or the
    /// view lost every touch, e.g. on backgrounding).
    func reset() {
        cancelAll()
        touches.removeAll()
        state = .idle
        pointer = nil
        pointerMovedAt = nil
    }

    // MARK: Internals

    private func releaseHeld() {
        switch state {
        case .trackpadDrag:
            sink?.button(.primary, pressed: false, at: ensurePointer())
        case .directDrag(_, _, let inside):
            sink?.button(.primary, pressed: false, at: inside)
        default:
            break
        }
    }

    /// How long a still touch stays a tap: until the hold or long press
    /// would take it over.
    private var tapLimit: Double {
        controlling && mode == .direct ? longPressTime : holdTime
    }

    private func tap(_ one: One, at location: CGPoint, time: Double, effects: inout [TouchEffect]) {
        guard controlling else {
            if let last = lastTap, time - last.time <= doubleTapTime,
               Self.distance(last.location, location) <= 44 {
                lastTap = nil
                if !mapper.resting {
                    mapper.reset()
                } else {
                    let exact = mapper.actualPixelsZoom(screenScale: screenScale)
                    mapper.setZoom(exact > 1.2 ? exact : 2.5, at: location)
                }
                effects.append(.viewport)
            } else {
                lastTap = (time, location)
            }
            return
        }
        let point: CGPoint
        switch mode {
        case .trackpad: point = ensurePointer()
        case .direct:
            guard let start = one.startPicture else { return }
            point = start
        }
        sink?.button(.primary, pressed: true, at: point)
        sink?.button(.primary, pressed: false, at: point)
    }

    private func endTwo(_ two: Two, time: Double, lifted: Bool, effects: inout [TouchEffect]) {
        switch two.kind {
        case .undecided:
            guard lifted, controlling, !two.moved, time - two.startTime <= twoFingerTapTime else { return }
            let point: CGPoint?
            switch mode {
            case .trackpad: point = ensurePointer()
            case .direct: point = mapper.toPicture(two.startCentroid)
            }
            if let point {
                sink?.button(.secondary, pressed: true, at: point)
                sink?.button(.secondary, pressed: false, at: point)
            }
        case .scroll:
            if lifted {
                scroller.lift(at: time)
            } else {
                scroller.stop()
            }
            if !scroller.coasting { sink?.scrollEnded() }
        case .zoom:
            // Settle on pixel-exact zoom when close to it, else on the
            // resting zoom when at or near it.
            let exact = mapper.actualPixelsZoom(screenScale: screenScale)
            let rest = mapper.restingZoom
            if abs(mapper.zoom - exact) / exact < 0.06 {
                mapper.setZoom(exact, at: two.lastCentroid)
                effects.append(.viewport)
            } else if mapper.zoom < rest * 1.05 {
                mapper.setZoom(rest, at: two.lastCentroid)
                effects.append(.viewport)
            }
        }
    }

    private func scrollPoint(_ centroid: CGPoint) -> CGPoint? {
        switch mode {
        case .trackpad: return ensurePointer()
        case .direct: return mapper.toPicture(centroid)
        }
    }

    /// The trackpad pointer, placed mid-view when unknown or off-picture.
    private func ensurePointer() -> CGPoint {
        if let pointer, pointer.x >= 0, pointer.y >= 0,
           pointer.x < mapper.picture.width, pointer.y < mapper.picture.height {
            return pointer
        }
        let visible = mapper.visiblePicture
        let seed = CGPoint(x: visible.midX.rounded(.down), y: visible.midY.rounded(.down))
        pointer = seed
        return seed
    }

    /// Whether the trackpad pointer has a position yet.
    var hasPointer: Bool { pointer != nil }

    /// Where to draw the host's cursor at `time` instead of where the host
    /// last reported it: the trackpad pointer while a finger is down in
    /// trackpad mode, or until `localPointerHold` after this device last
    /// moved it, so the cursor moves with the finger. nil (view-only, direct
    /// mode, or idle): draw the host's report, the truth once it caught up.
    func localPointer(at time: Double) -> CGPoint? {
        guard controlling, mode == .trackpad, let pointer else { return nil }
        if !touches.isEmpty { return pointer }
        guard let moved = pointerMovedAt, time - moved < localPointerHold else { return nil }
        return pointer
    }

    /// Place the trackpad pointer (e.g. at the host's reported cursor).
    func seedPointer(_ point: CGPoint) {
        guard point.x >= 0, point.y >= 0, point.x < mapper.picture.width,
              point.y < mapper.picture.height else { return }
        pointer = point
    }

    private func movePointer(_ delta: CGVector, interval: Double, at time: Double,
                             effects: inout [TouchEffect]) {
        let start = ensurePointer()
        let travel = acceleration.travel(delta, interval: interval, scale: mapper.scale)
        // The pointer is ours to keep inside the picture (unlike a touch).
        let limit = CGSize(width: mapper.picture.width - 1.0 / 256,
                           height: mapper.picture.height - 1.0 / 256)
        let next = CGPoint(x: min(max(0, start.x + travel.dx), limit.width),
                           y: min(max(0, start.y + travel.dy), limit.height))
        pointer = next
        if next != start {
            pointerMovedAt = time
            sink?.pointer(to: next)
        }
        if mapper.reveal(next, margin: edgeMargin) { effects.append(.viewport) }
    }

    /// How hard a finger presses into the content edge, per axis, -1...1
    /// (positive moves the picture right/down to reveal the left/top).
    private func edgePush(_ location: CGPoint) -> CGVector {
        guard mapper.zoom > 1 else { return .zero }
        let content = mapper.content
        func push(_ value: CGFloat, _ low: CGFloat, _ high: CGFloat) -> CGFloat {
            if value < low + edgeMargin { return min(1, (low + edgeMargin - value) / edgeMargin) }
            if value > high - edgeMargin { return -min(1, (value - (high - edgeMargin)) / edgeMargin) }
            return 0
        }
        return CGVector(dx: push(location.x, content.minX, content.maxX),
                        dy: push(location.y, content.minY, content.maxY))
    }

    static func distance(_ a: CGPoint, _ b: CGPoint) -> CGFloat {
        ((a.x - b.x) * (a.x - b.x) + (a.y - b.y) * (a.y - b.y)).squareRoot()
    }
    static func midpoint(_ a: CGPoint, _ b: CGPoint) -> CGPoint {
        CGPoint(x: (a.x + b.x) / 2, y: (a.y + b.y) / 2)
    }
}
