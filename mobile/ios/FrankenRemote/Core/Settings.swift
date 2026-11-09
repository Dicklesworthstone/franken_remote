import Foundation

/// User settings (stored in user defaults; nothing secret).
final class AppSettings: ObservableObject {
    private let defaults: UserDefaults

    @Published var touchMode: TouchMode { didSet { defaults.set(touchMode.rawValue, forKey: Keys.touchMode) } }
    /// 0.5...2.
    @Published var pointerSpeed: Double { didSet { defaults.set(pointerSpeed, forKey: Keys.pointerSpeed) } }
    /// Lines per 18 points of finger travel, 0.5...3.
    @Published var scrollSpeed: Double { didSet { defaults.set(scrollSpeed, forKey: Keys.scrollSpeed) } }
    @Published var naturalScrolling: Bool { didSet { defaults.set(naturalScrolling, forKey: Keys.natural) } }
    /// Ask for control as soon as the first picture is on screen.
    @Published var controlOnConnect: Bool { didSet { defaults.set(controlOnConnect, forKey: Keys.controlOnConnect) } }
    @Published var haptics: Bool { didSet { defaults.set(haptics, forKey: Keys.haptics) } }
    @Published var showStats: Bool { didSet { defaults.set(showStats, forKey: Keys.showStats) } }
    @Published var defaultPort: Int { didSet { defaults.set(defaultPort, forKey: Keys.defaultPort) } }

    /// frd's default listening port.
    static let frdDefaultPort = 8443

    private enum Keys {
        static let touchMode = "touchMode"
        static let pointerSpeed = "pointerSpeed"
        static let scrollSpeed = "scrollSpeed"
        static let natural = "naturalScrolling"
        static let controlOnConnect = "controlOnConnect"
        static let haptics = "haptics"
        static let showStats = "showStats"
        static let defaultPort = "defaultPort"
    }

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        touchMode = TouchMode(rawValue: defaults.string(forKey: Keys.touchMode) ?? "") ?? .trackpad
        pointerSpeed = (defaults.object(forKey: Keys.pointerSpeed) as? Double) ?? 1
        scrollSpeed = (defaults.object(forKey: Keys.scrollSpeed) as? Double) ?? 1
        naturalScrolling = (defaults.object(forKey: Keys.natural) as? Bool) ?? true
        controlOnConnect = (defaults.object(forKey: Keys.controlOnConnect) as? Bool) ?? false
        haptics = (defaults.object(forKey: Keys.haptics) as? Bool) ?? true
        showStats = (defaults.object(forKey: Keys.showStats) as? Bool) ?? false
        let port = (defaults.object(forKey: Keys.defaultPort) as? Int) ?? Self.frdDefaultPort
        defaultPort = (1...65535).contains(port) ? port : Self.frdDefaultPort
    }
}
