import MetalKit
import UIKit

/// The session's screen: the Metal picture, every touch, the hardware
/// keyboard and the software keyboard with its extra-keys bar.
///
/// Raw touches go to `TouchInterpreter` (no gesture recognizers: they would
/// delay taps and fight each other). Hardware key presses become HID usages
/// one-for-one with no client-side repeat; software keyboard characters
/// become US-layout key presses. Unicode and negotiated multi-character paste
/// go to the host's text input once, without queueing, splitting or retry.
final class RemoteSurfaceView: UIView, UIKeyInput {
    private var metalView = MTKView()
    let interpreter = TouchInterpreter()
    private var renderer: MetalRenderer?
    private var feedback = UIImpactFeedbackGenerator(style: .light)
    private lazy var extraKeys = ExtraKeysBar(frame: CGRect(x: 0, y: 0, width: 320, height: 46))
    private var modifiers = StickyModifiers()
    private var softwareKeyboard = false
    private var heldKeys: Set<UInt16> = []
    /// Software-keyboard input waiting for the host's input queue (a paste
    /// arrives faster than the host takes keys): every later refresh retains
    /// the original Rust capture, view and exclusive deadline. Never logged.
    private var typing = TypingQueue()
    /// How far the keyboard (with its extra-keys bar) covers the view's
    /// bottom edge: the picture's content area stops above it.
    private var keyboardOverlap: CGFloat = 0
    /// The exact frame whose live owner supplied the current input mapping.
    private var presentationFrame: UInt64?

    weak var session: RemoteSession? {
        willSet {
            if session !== newValue {
                releaseKeys()
                modifiers.clear()
                setSoftwareKeyboard(false)
                extraKeys.show(modifiers)
                interpreter.replaceSink(newValue)
                presentationFrame = nil
            }
        }
        didSet {
            renderer?.session = session
            renderer?.reset()
        }
    }
    var hapticsEnabled = true
    /// Tells SwiftUI when the software keyboard was dismissed by the system.
    var onKeyboardHidden: (() -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = .black
        metalView.device = MTLCreateSystemDefaultDevice()
        installMetalView()
        if let renderer = MetalRenderer(view: metalView) {
            self.renderer = renderer
            metalView.delegate = renderer
            renderer.mapper = { [weak self] in self?.interpreter.mapper ?? PictureMapper() }
            renderer.onRefresh = { [weak self] time in self?.refresh(time) }
        }
        extraKeys.onKey = { [weak self] usage in self?.typeExtra(usage) }
        extraKeys.onModifier = { [weak self] modifier in
            guard let self else { return }
            modifiers.tap(modifier)
            extraKeys.show(modifiers)
        }
        extraKeys.onDismiss = { [weak self] in self?.setSoftwareKeyboard(false) }
        accessibilityLabel = "Remote screen"
        let center = NotificationCenter.default
        center.addObserver(self, selector: #selector(keyboardFrameChanged(_:)),
                           name: UIResponder.keyboardWillChangeFrameNotification, object: nil)
        center.addObserver(self, selector: #selector(keyboardWillHide(_:)),
                           name: UIResponder.keyboardWillHideNotification, object: nil)
    }

    private func installMetalView() {
        metalView.isUserInteractionEnabled = false
        metalView.colorPixelFormat = .bgra8Unorm
        metalView.framebufferOnly = true
        metalView.clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        metalView.enableSetNeedsDisplay = false
        metalView.isPaused = false
        metalView.autoResizeDrawable = true
        addSubview(metalView)
        metalView.delegate = renderer
        setNeedsLayout()
    }

    @objc private func keyboardFrameChanged(_ note: Notification) {
        guard let window,
              let frame = note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect else { return }
        let local = convert(frame, from: window.screen.coordinateSpace)
        let overlap = local.intersects(bounds) ? max(0, bounds.maxY - local.minY) : 0
        if overlap != keyboardOverlap {
            keyboardOverlap = overlap
            setNeedsLayout()
        }
    }

    @objc private func keyboardWillHide(_ note: Notification) {
        if keyboardOverlap != 0 {
            keyboardOverlap = 0
            setNeedsLayout()
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used from a storyboard") }

    override func layoutSubviews() {
        super.layoutSubviews()
        metalView.frame = bounds
        // Draw at the panel's own pixel density (nativeScale; it differs
        // from `scale` on some phones), so one picture pixel can be exactly
        // one device pixel.
        let deviceScale = window?.screen.nativeScale ?? traitCollection.displayScale
        if metalView.contentScaleFactor != deviceScale { metalView.contentScaleFactor = deviceScale }
        let insets = Insets(top: safeAreaInsets.top, left: safeAreaInsets.left,
                            bottom: max(safeAreaInsets.bottom, keyboardOverlap),
                            right: safeAreaInsets.right)
        updateViewport { $0.setGeometry(viewSize: bounds.size, insets: insets, deviceScale: deviceScale) }
        interpreter.screenScale = deviceScale
        // Match the display's refresh (120 Hz on ProMotion phones, with
        // CADisableMinimumFrameDurationOnPhone in the Info.plist).
        metalView.preferredFramesPerSecond = window?.screen.maximumFramesPerSecond ?? 60
    }

    override func safeAreaInsetsDidChange() {
        super.safeAreaInsetsDidChange()
        setNeedsLayout()
    }

    /// Once per display refresh: events, retained-frame geometry, timers.
    private func refresh(_ time: CFTimeInterval) {
        guard let session else { return }
        session.pump()
        guard synchronizePresentation(at: time) else { return }
        let controlling = session.phase == .controlled
        if interpreter.controlling != controlling {
            interpreter.controlling = controlling
            if !controlling { releaseKeys() }
            if controlling, !isFirstResponder { becomeFirstResponder() }
        }
        // The trackpad pointer starts where the host's cursor is, so the
        // first tap clicks under the cursor the user sees, and follows it
        // while this device is not moving it (the host moved its cursor). The
        // cursor is drawn at the pointer while the finger drives it.
        let local = interpreter.localPointer(at: time)
        if controlling, let cursor = renderer?.hostCursor, presentationIsCurrent() {
            if let pointer = interpreter.pointer {
                if local == nil, hypot(pointer.x - cursor.x, pointer.y - cursor.y) > 1.5 {
                    interpreter.seedPointer(cursor)
                }
            } else {
                interpreter.seedPointer(cursor)
            }
        }
        guard presentationIsCurrent() else { retirePresentation(); return }
        renderer?.localCursor = local
        if !typing.isEmpty { drainTyping() }
        handle(interpreter.tick(time: time))
    }

    /// A lossy VIDEO event never establishes an input transform. The copied
    /// frame and its original owner must agree before gestures are serviced.
    private func synchronizePresentation(at time: CFTimeInterval) -> Bool {
        guard let session, let renderer, let next = renderer.prepareFrame() else {
            retirePresentation()
            return false
        }
        let previous = presentationFrame
        let replaced = previous.map { session.display(forFrame: $0) == nil } ?? false
        guard session.display(forFrame: next.frame) == next.display else {
            retirePresentation()
            return false
        }
        let changed = previous == nil || replaced
            || interpreter.mapper.picture != next.picture || interpreter.mapper.display != next.display
        if changed { retirePresentation(at: time) }
        var mapper = interpreter.mapper
        mapper.setPicture(next.picture, display: next.display, frame: next.frame)
        interpreter.mapper = mapper
        presentationFrame = next.frame
        return mapper.isReady
    }

    private func presentationIsCurrent() -> Bool {
        guard let session, let frame = presentationFrame else { return false }
        return interpreter.mapper.frame == frame
            && session.display(forFrame: frame) == interpreter.mapper.display
    }

    /// The original Rust owner releases held host state. Discard local work
    /// without sending an old gesture or key release into a successor.
    private func retirePresentation(at time: CFTimeInterval? = nil) {
        let hadPresentation = presentationFrame != nil
        interpreter.retirePresentation(at: time)
        presentationFrame = nil
        renderer?.localCursor = nil
        if hadPresentation {
            typing.clear()
            heldKeys.removeAll()
            modifiers.clear()
            extraKeys.show(modifiers)
        }
    }

    private func handle(_ effects: [TouchEffect]) {
        if hapticsEnabled, effects.contains(.haptic) {
            feedback.impactOccurred()
        }
    }

    // MARK: Touches

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard synchronizePresentation(at: CACurrentMediaTime()) else { return }
        feedback.prepare()
        for touch in touches {
            guard presentationIsCurrent() else { retirePresentation(); return }
            handle(interpreter.began(id(touch), at: touch.location(in: self), time: touch.timestamp))
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard synchronizePresentation(at: CACurrentMediaTime()) else { return }
        for touch in touches {
            // Coalesced samples keep fast strokes smooth on 120 Hz screens.
            let samples = event?.coalescedTouches(for: touch) ?? [touch]
            for sample in samples {
                guard presentationIsCurrent() else { retirePresentation(); return }
                handle(interpreter.moved(id(touch), to: sample.location(in: self), time: sample.timestamp))
            }
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard synchronizePresentation(at: CACurrentMediaTime()) else { return }
        for touch in touches {
            guard presentationIsCurrent() else { retirePresentation(); return }
            handle(interpreter.ended(id(touch), at: touch.location(in: self), time: touch.timestamp))
        }
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard presentationIsCurrent() else { retirePresentation(); return }
        for touch in touches {
            handle(interpreter.cancelled(id(touch), time: touch.timestamp))
        }
    }

    private func id(_ touch: UITouch) -> Int { ObjectIdentifier(touch).hashValue }

    // MARK: Zoom controls

    private func updateViewport(_ update: (inout PictureMapper) -> Void) {
        let time = CACurrentMediaTime()
        if presentationFrame != nil, !presentationIsCurrent() { retirePresentation(at: time) }
        interpreter.updateViewport(at: time, update)
    }

    func zoomToFit() {
        updateViewport { $0.reset() }
    }

    /// One picture pixel per device pixel, centred on the view.
    func zoomToActualPixels() {
        let scale = interpreter.screenScale
        updateViewport { mapper in
            let content = mapper.content
            mapper.setZoom(mapper.actualPixelsZoom(screenScale: scale),
                           at: CGPoint(x: content.midX, y: content.midY))
        }
    }

    // MARK: Keyboard

    override var canBecomeFirstResponder: Bool { true }

    /// The software keyboard shows only when asked; otherwise an empty input
    /// view keeps hardware-key focus without covering the picture.
    override var inputView: UIView? { softwareKeyboard ? nil : emptyInput }
    override var inputAccessoryView: UIView? { softwareKeyboard ? extraKeys : nil }
    private let emptyInput = UIView(frame: .zero)

    func setSoftwareKeyboard(_ shown: Bool) {
        guard shown != softwareKeyboard || (shown && !isFirstResponder) else { return }
        softwareKeyboard = shown
        if shown {
            extraKeys.show(modifiers)
            if isFirstResponder { reloadInputViews() } else { becomeFirstResponder() }
        } else {
            typing.clear()
            modifiers.clear()
            if isFirstResponder { reloadInputViews() }
            onKeyboardHidden?()
        }
    }

    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        if resigned {
            releaseKeys()
            if softwareKeyboard {
                softwareKeyboard = false
                onKeyboardHidden?()
            }
        }
        return resigned
    }

    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardType: UIKeyboardType = .default
    var returnKeyType: UIReturnKeyType = .default

    var hasText: Bool { true }

    func insertText(_ text: String) {
        guard let session, !text.isEmpty else { return }
        // Capture before validation/classification can wait or the source can
        // change. A direct text callback discards this unused physical token.
        let physical = TypingCapture.capture(into: session)
        guard UnicodeCommit.valid(text) else {
            session.notice = "That keyboard callback is too large or contains an unsupported character. Nothing was sent."
            return
        }
        if UnicodeCommit.usesText(for: text, supported: session.supportsText,
                                 modifiersActive: !modifiers.active.isEmpty) {
            // One original UIKeyInput completion. Refuse the whole callback
            // before queuing a prefix or consuming any sticky modifier.
            let status = UnicodeCommit.submit(
                text, pendingKeys: !typing.isEmpty, heldKeys: !heldKeys.isEmpty,
                modifiersActive: !modifiers.active.isEmpty, into: session
            )
            if let notice = UnicodeCommit.notice(for: status) { session.notice = notice }
            return
        }
        // Capture BEFORE converting the completed callback. Mapping time and
        // every busy wait consume this same Rust-owned source/view deadline.
        let capture: TypingCapture
        switch physical {
        case .success(let original): capture = original
        case .failure:
            session.notice = "Typing was not sent because the current input view is unavailable."
            return
        }
        var items: [TypingQueue.Item] = []
        var nextModifiers = modifiers
        for character in text {
            let held = nextModifiers.active.map(\.usage)
            if let stroke = KeyboardMap.stroke(for: character) {
                items.append(.stroke(stroke.usage, modifiers: stroke.shift ? held + [HIDUsage.leftShift] : held))
                nextModifiers.consume()
            }
        }
        if enqueue(items, capture: capture) { modifiers = nextModifiers }
        extraKeys.show(modifiers)
    }

    func deleteBackward() {
        typeExtra(HIDUsage.backspace)
    }

    private func typeExtra(_ usage: UInt16) {
        guard let session else { return }
        switch TypingCapture.capture(into: session) {
        case .success(let capture):
            if enqueue([.stroke(usage, modifiers: modifiers.active.map(\.usage))], capture: capture) {
                modifiers.consume()
            }
        case .failure:
            session.notice = "That key was not sent because the current input view is unavailable."
        }
        extraKeys.show(modifiers)
    }

    @discardableResult
    private func enqueue(_ items: [TypingQueue.Item], capture: TypingCapture) -> Bool {
        guard typing.append(items, capture: capture) else {
            session?.notice = "That keyboard callback does not fit the typing queue. None of it was sent."
            return false
        }
        drainTyping()
        return true
    }

    /// Send queued typing until the host's input queue backs up again.
    private func drainTyping() {
        guard let session else {
            typing.clear()
            return
        }
        let outcome = typing.drain(into: session)
        if outcome.refusal != nil {
            session.notice = "Queued typing lost its original input deadline or view. Remaining keys were discarded."
        }
    }

    // Hardware keyboards: one press, one release, no repeat.
    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var unhandled = Set<UIPress>()
        for press in presses {
            guard let usage = hidUsage(press), interpreter.controlling, let session else {
                unhandled.insert(press)
                continue
            }
            if session.key(usage, pressed: true) == .sent { heldKeys.insert(usage) }
        }
        if !unhandled.isEmpty { super.pressesBegan(unhandled, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        releasePresses(presses, event: event, cancelled: false)
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        releasePresses(presses, event: event, cancelled: true)
    }

    private func releasePresses(_ presses: Set<UIPress>, event: UIPressesEvent?, cancelled: Bool) {
        var unhandled = Set<UIPress>()
        for press in presses {
            guard let usage = hidUsage(press), heldKeys.contains(usage) else {
                unhandled.insert(press)
                continue
            }
            heldKeys.remove(usage)
            session?.key(usage, pressed: false)
        }
        if unhandled.isEmpty { return }
        if cancelled {
            super.pressesCancelled(unhandled, with: event)
        } else {
            super.pressesEnded(unhandled, with: event)
        }
    }

    private func hidUsage(_ press: UIPress) -> UInt16? {
        guard let key = press.key else { return nil }
        let raw = key.keyCode.rawValue
        guard raw >= 0, raw <= Int(UInt16.max) else { return nil }
        let usage = UInt16(raw)
        return HIDUsage.isSendable(usage) ? usage : nil
    }

    /// Release every key this view pressed (focus loss, control end), and
    /// drop typing that has not gone yet.
    func releaseKeys() {
        heldKeys.removeAll()
        typing.clear()
        session?.releaseAllKeys()
    }

    /// Metal's retained drawable and the session's visibility have the same
    /// lifetime. Cover the remote pixels even if iOS stops display callbacks
    /// before the renderer can submit its black drawable.
    func setSurfaceVisible(_ visible: Bool) {
        session?.setVisible(visible)
        renderer?.setVisible(visible)
        if visible, metalView.isHidden {
            // iOS can stop servicing drawables while hidden. A fresh layer
            // cannot flash the previous desktop before its first draw.
            let previous = metalView
            previous.delegate = nil
            previous.removeFromSuperview()
            metalView = MTKView(frame: bounds, device: previous.device)
            installMetalView()
        } else if !visible {
            metalView.isHidden = true
            suspend()
        }
    }

    /// The app left the foreground or the view went away.
    private func suspend() {
        // The next visibility interval must establish its own touch-time
        // floor, even when the first drawable has the same size and frame.
        retirePresentation(at: CACurrentMediaTime())
        releaseKeys()
        setSoftwareKeyboard(false)
    }
}
