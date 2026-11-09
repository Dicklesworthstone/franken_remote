import UIKit

/// The row above the software keyboard: keys a phone keyboard lacks, and
/// sticky modifiers (tap: next key only; tap again: locked; again: off).
final class ExtraKeysBar: UIInputView {
    var onKey: ((UInt16) -> Void)?
    var onModifier: ((StickyModifiers.Modifier) -> Void)?
    var onDismiss: (() -> Void)?

    private let scroll = UIScrollView()
    private let stack = UIStackView()
    private var modifierButtons: [StickyModifiers.Modifier: UIButton] = [:]

    override init(frame: CGRect, inputViewStyle: UIInputView.Style) {
        super.init(frame: frame, inputViewStyle: inputViewStyle)
        build()
    }

    convenience init(frame: CGRect) {
        self.init(frame: frame, inputViewStyle: .keyboard)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used from a storyboard") }

    private func build() {
        allowsSelfSizing = true
        scroll.showsHorizontalScrollIndicator = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        stack.axis = .horizontal
        stack.spacing = 6
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(scroll)
        scroll.addSubview(stack)
        let dismiss = button(systemImage: "keyboard.chevron.compact.down", label: "Hide keyboard") { [weak self] in
            self?.onDismiss?()
        }
        dismiss.translatesAutoresizingMaskIntoConstraints = false
        addSubview(dismiss)
        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: 46),
            dismiss.trailingAnchor.constraint(equalTo: safeAreaLayoutGuide.trailingAnchor, constant: -6),
            dismiss.centerYAnchor.constraint(equalTo: centerYAnchor),
            scroll.leadingAnchor.constraint(equalTo: safeAreaLayoutGuide.leadingAnchor, constant: 6),
            scroll.trailingAnchor.constraint(equalTo: dismiss.leadingAnchor, constant: -6),
            scroll.topAnchor.constraint(equalTo: topAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor),
            stack.centerYAnchor.constraint(equalTo: scroll.centerYAnchor),
            stack.heightAnchor.constraint(equalToConstant: 36),
        ])
        add(key: "esc", HIDUsage.escape)
        add(key: "tab", HIDUsage.tab)
        for modifier in StickyModifiers.Modifier.allCases {
            let button = button(title: modifier.title, label: modifier.title) { [weak self] in
                self?.onModifier?(modifier)
            }
            modifierButtons[modifier] = button
            stack.addArrangedSubview(button)
        }
        add(image: "arrow.left", label: "Left arrow", HIDUsage.left)
        add(image: "arrow.up", label: "Up arrow", HIDUsage.up)
        add(image: "arrow.down", label: "Down arrow", HIDUsage.down)
        add(image: "arrow.right", label: "Right arrow", HIDUsage.right)
        add(key: "del", HIDUsage.deleteForward, label: "Forward delete")
        add(key: "home", HIDUsage.home)
        add(key: "end", HIDUsage.end)
        add(key: "pgup", HIDUsage.pageUp, label: "Page up")
        add(key: "pgdn", HIDUsage.pageDown, label: "Page down")
        for n in 1...12 {
            if let usage = HIDUsage.function(n) { add(key: "F\(n)", usage) }
        }
    }

    private func add(key title: String, _ usage: UInt16, label: String? = nil) {
        stack.addArrangedSubview(button(title: title, label: label ?? title) { [weak self] in
            self?.onKey?(usage)
        })
    }

    private func add(image: String, label: String, _ usage: UInt16) {
        stack.addArrangedSubview(button(systemImage: image, label: label) { [weak self] in
            self?.onKey?(usage)
        })
    }

    private func button(title: String? = nil, systemImage: String? = nil, label: String,
                        action: @escaping () -> Void) -> UIButton {
        var configuration = UIButton.Configuration.gray()
        configuration.cornerStyle = .medium
        configuration.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 10, bottom: 6, trailing: 10)
        if let title {
            var text = AttributedString(title)
            text.font = .systemFont(ofSize: 15, weight: .medium)
            configuration.attributedTitle = text
        }
        if let systemImage { configuration.image = UIImage(systemName: systemImage) }
        let button = UIButton(configuration: configuration, primaryAction: UIAction { _ in action() })
        button.accessibilityLabel = label
        button.setContentHuggingPriority(.required, for: .horizontal)
        return button
    }

    /// Show each modifier's level: latched tinted, locked filled.
    func show(_ modifiers: StickyModifiers) {
        for (modifier, button) in modifierButtons {
            var configuration: UIButton.Configuration
            switch modifiers.level(modifier) {
            case .off: configuration = .gray()
            case .latched: configuration = .tinted()
            case .locked: configuration = .filled()
            }
            configuration.cornerStyle = .medium
            configuration.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 10, bottom: 6, trailing: 10)
            var text = AttributedString(modifier.title)
            text.font = .systemFont(ofSize: 15, weight: .medium)
            configuration.attributedTitle = text
            button.configuration = configuration
            button.accessibilityValue = {
                switch modifiers.level(modifier) {
                case .off: return "off"
                case .latched: return "next key"
                case .locked: return "locked"
                }
            }()
        }
    }
}
