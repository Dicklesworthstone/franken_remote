import MetalKit
import UIKit

/// The session's screen: the Metal picture, every touch, the hardware
/// keyboard and the software keyboard with its extra-keys bar.
///
/// Raw touches go to `TouchInterpreter` (no gesture recognizers: they would
/// delay taps and fight each other). Hardware key presses become HID usages
/// one-for-one with no client-side repeat; software keyboard characters
/// become US-layout key presses, and anything else goes to the host's text
/// input only if the host granted it (never a guessed layout).
final class RemoteSurfaceView: UIView, UIKeyInput {
    let metalView = MTKView()
    let interpreter = TouchInterpreter()
    private var renderer: MetalRenderer?
    private var feedback = UIImpactFeedbackGenerator(style: .light)
    private lazy var extraKeys = ExtraKeysBar(frame: CGRect(x: 0, y: 0, width: 320, height: 46))
    private var modifiers = StickyModifiers()
    private var softwareKeyboard = false
    private var heldKeys: Set<UInt16> = []
    /// Software-keyboard input waiting for the host's input queue (a paste
    /// arrives faster than the host takes keys): sent in order on later
    /// refreshes. Bounded; never logged.
    private var typing = TypingQueue()
    /// How far the keyboard (with its extra-keys bar) covers the view's
    /// bottom edge: the picture's content area stops above it.
    private var keyboardOverlap: CGFloat = 0

    weak var session: RemoteSession? {
        didSet {
            interpreter.sink = session
            renderer?.session = session
            renderer?.reset()
            interpreter.reset()
        }
    }
    var hapticsEnabled = true
    /// Tells SwiftUI when the software keyboard was dismissed by the system.
    var onKeyboardHidden: (() -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = .black
        metalView.isUserInteractionEnabled = false
        metalView.colorPixelFormat = .bgra8Unorm
        metalView.framebufferOnly = true
        metalView.clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        metalView.enableSetNeedsDisplay = false
        metalView.isPaused = false
        metalView.autoResizeDrawable = true
        metalView.device = MTLCreateSystemDefaultDevice()
        addSubview(metalView)
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
        var mapper = interpreter.mapper
        mapper.setGeometry(viewSize: bounds.size,
                           insets: Insets(top: safeAreaInsets.top, left: safeAreaInsets.left,
                                          bottom: max(safeAreaInsets.bottom, keyboardOverlap),
                                          right: safeAreaInsets.right),
                           deviceScale: deviceScale)
        interpreter.mapper = mapper
        interpreter.screenScale = deviceScale
        // Match the display's refresh (120 Hz on ProMotion phones, with
        // CADisableMinimumFrameDurationOnPhone in the Info.plist).
        metalView.preferredFramesPerSecond = window?.screen.maximumFramesPerSecond ?? 60
    }

    override func safeAreaInsetsDidChange() {
        super.safeAreaInsetsDidChange()
        setNeedsLayout()
    }

    /// Once per display refresh: session events, the picture size, timers.
    private func refresh(_ time: CFTimeInterval) {
        guard let session else { return }
        session.pump()
        if session.picture != .zero, session.picture != interpreter.mapper.picture {
            var mapper = interpreter.mapper
            mapper.setPicture(session.picture)
            interpreter.mapper = mapper
            renderer?.reset()
        }
        let controlling = session.phase == .controlled
        if interpreter.controlling != controlling {
            interpreter.controlling = controlling
            if !controlling { releaseKeys() }
            if controlling, !isFirstResponder { becomeFirstResponder() }
        }
        // The trackpad pointer starts where the host's cursor is, so the
        // first tap clicks under the cursor the user sees.
        if controlling, !interpreter.hasPointer, let cursor = renderer?.hostCursor {
            interpreter.seedPointer(cursor)
        }
        if !typing.isEmpty { drainTyping() }
        handle(interpreter.tick(time: time))
    }

    private func handle(_ effects: [TouchEffect]) {
        if hapticsEnabled, effects.contains(.haptic) {
            feedback.impactOccurred()
        }
    }

    // MARK: Touches

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        feedback.prepare()
        for touch in touches {
            handle(interpreter.began(id(touch), at: touch.location(in: self), time: touch.timestamp))
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            // Coalesced samples keep fast strokes smooth on 120 Hz screens.
            let samples = event?.coalescedTouches(for: touch) ?? [touch]
            for sample in samples {
                handle(interpreter.moved(id(touch), to: sample.location(in: self), time: sample.timestamp))
            }
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            handle(interpreter.ended(id(touch), at: touch.location(in: self), time: touch.timestamp))
        }
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            handle(interpreter.cancelled(id(touch), time: touch.timestamp))
        }
    }

    private func id(_ touch: UITouch) -> Int { ObjectIdentifier(touch).hashValue }

    // MARK: Zoom controls

    func zoomToFit() {
        var mapper = interpreter.mapper
        mapper.reset()
        interpreter.mapper = mapper
    }

    /// One picture pixel per device pixel, centred on the view.
    func zoomToActualPixels() {
        var mapper = interpreter.mapper
        let content = mapper.content
        mapper.setZoom(mapper.actualPixelsZoom(screenScale: interpreter.screenScale),
                       at: CGPoint(x: content.midX, y: content.midY))
        interpreter.mapper = mapper
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
    var keyboardType: UIKeyboardType = .asciiCapable
    var returnKeyType: UIReturnKeyType = .default

    var hasText: Bool { true }

    func insertText(_ text: String) {
        guard session != nil else { return }
        var items: [TypingQueue.Item] = []
        for character in text {
            let held = modifiers.active.map(\.usage)
            if let stroke = KeyboardMap.stroke(for: character) {
                items.append(.stroke(stroke.usage, modifiers: stroke.shift ? held + [HIDUsage.leftShift] : held))
                modifiers.consume()
            } else {
                items.append(.text(String(character)))
            }
        }
        enqueue(items)
        extraKeys.show(modifiers)
    }

    func deleteBackward() {
        typeExtra(HIDUsage.backspace)
    }

    private func typeExtra(_ usage: UInt16) {
        guard session != nil else { return }
        enqueue([.stroke(usage, modifiers: modifiers.active.map(\.usage))])
        modifiers.consume()
        extraKeys.show(modifiers)
    }

    private func enqueue(_ items: [TypingQueue.Item]) {
        if !typing.append(items) {
            session?.notice = "That's too much text to type at once. Only the start was sent."
        }
        drainTyping()
    }

    /// Send queued typing until the host's input queue backs up again.
    private func drainTyping() {
        guard let session else {
            typing.clear()
            return
        }
        let outcome = typing.drain(into: session)
        if outcome.unsupported {
            // Content-free: never quote what was typed.
            session.notice = "Some characters can't be sent: this host takes key presses only, and the app won't guess the host's keyboard layout."
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

    /// The app left the foreground or the view went away.
    func suspend() {
        interpreter.reset()
        releaseKeys()
        setSoftwareKeyboard(false)
    }
}
