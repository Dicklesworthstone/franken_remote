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
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used from a storyboard") }

    override func layoutSubviews() {
        super.layoutSubviews()
        metalView.frame = bounds
        var mapper = interpreter.mapper
        mapper.viewSize = bounds.size
        mapper.insets = Insets(top: safeAreaInsets.top, left: safeAreaInsets.left,
                               bottom: safeAreaInsets.bottom, right: safeAreaInsets.right)
        mapper.clamp()
        interpreter.mapper = mapper
        interpreter.screenScale = window?.screen.scale ?? traitCollection.displayScale
        // Match the display's refresh (120 Hz on ProMotion phones).
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
        guard let session else { return }
        var unsupported: [Character] = []
        for character in text {
            let held = modifiers.active.map(\.usage)
            if let stroke = KeyboardMap.stroke(for: character) {
                let mods = stroke.shift ? held + [HIDUsage.leftShift] : held
                KeyTyper.type(stroke.usage, modifiers: mods, into: session)
                modifiers.consume()
            } else if session.text(String(character)) == .unsupported {
                unsupported.append(character)
            }
        }
        extraKeys.show(modifiers)
        if let first = unsupported.first {
            session.notice = "“\(first)” can't be sent: this host takes key presses only, and the app won't guess the host's keyboard layout."
        }
    }

    func deleteBackward() {
        typeExtra(HIDUsage.backspace)
    }

    private func typeExtra(_ usage: UInt16) {
        guard let session else { return }
        KeyTyper.type(usage, modifiers: modifiers.active.map(\.usage), into: session)
        modifiers.consume()
        extraKeys.show(modifiers)
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

    /// Release every key this view pressed (focus loss, control end).
    func releaseKeys() {
        heldKeys.removeAll()
        session?.releaseAllKeys()
    }

    /// The app left the foreground or the view went away.
    func suspend() {
        interpreter.reset()
        releaseKeys()
        setSoftwareKeyboard(false)
    }
}
