import Combine
import Network
import SwiftUI

/// App-wide state: saved hosts, settings, the one live session, the network
/// path, and the reconnect offer after the app returns to the foreground.
@MainActor
final class AppModel: ObservableObject {
    let store: HostStore
    let settings: AppSettings
    /// No machine directory exists on iOS today (see `HostDirectory`).
    let directory: HostDirectory? = nil

    @Published private(set) var session: RemoteSession?
    /// A host whose session the app closed when it went to the background.
    @Published var reconnectOffer: SavedHost?
    @Published var startError: String?
    @Published private(set) var online = true
    @Published private(set) var pathDescription = ""

    private let monitor = NWPathMonitor()
    private var subscriptions: Set<AnyCancellable> = []
    private var lastPath: String?

    init(store: HostStore, settings: AppSettings) {
        self.store = store
        self.settings = settings
        monitor.pathUpdateHandler = { [weak self] path in
            let online = path.status == .satisfied
            let kind: String
            if path.usesInterfaceType(.wifi) {
                kind = "Wi-Fi"
            } else if path.usesInterfaceType(.cellular) {
                kind = "Cellular"
            } else if path.usesInterfaceType(.wiredEthernet) {
                kind = "Ethernet"
            } else {
                kind = online ? "Online" : "Offline"
            }
            Task { @MainActor in self?.pathChanged(online: online, kind: kind) }
        }
        monitor.start(queue: DispatchQueue(label: "fr.network-path"))
    }

    convenience init() {
        self.init(store: HostStore(isValidName: { name in name.withCString { fr_host_name_valid($0) } }),
                  settings: AppSettings())
    }

    private func pathChanged(online: Bool, kind: String) {
        let changed = lastPath != nil && lastPath != kind
        lastPath = kind
        self.online = online
        pathDescription = kind
        guard let session, !session.phase.isEnded else { return }
        if !online {
            session.notice = "This device is offline. The session resumes if the network returns soon."
        } else if changed {
            session.notice = "Network changed to \(kind). Reconnecting if needed…"
        }
    }

    /// Start a session (closing any other). `control` asks for a
    /// control-capable session; control itself is requested separately.
    func connect(_ host: SavedHost, control: Bool) {
        reconnectOffer = nil
        startError = nil
        session?.close()
        session = nil
        subscriptions.removeAll()
        do {
            let next = try RemoteSession(host: host, control: control,
                                         requestControl: control && settings.controlOnConnect)
            next.$observedPin
                .compactMap { $0 }
                .sink { [weak self] pin in self?.store.recordPin(pin, for: host.id) }
                .store(in: &subscriptions)
            store.markConnected(host.id)
            session = next
        } catch RemoteSession.StartError.invalid {
            startError = "That machine name or its saved pin isn't valid. Edit the machine and try again."
        } catch {
            startError = "The app could not start a session right now."
        }
    }

    func disconnect() {
        session?.close()
        session = nil
        subscriptions.removeAll()
    }

    /// The scene moved between foreground and background.
    func scenePhaseChanged(_ phase: ScenePhase) {
        switch phase {
        case .background:
            // Never keep control (or a view) running unseen: close, and offer
            // to reconnect when the user comes back.
            if let session, !session.phase.isEnded {
                reconnectOffer = session.host
                session.close()
            }
            session = nil
            subscriptions.removeAll()
        case .inactive, .active:
            break
        @unknown default:
            break
        }
    }
}

#if DEBUG
extension AppModel {
    /// Debug builds only, for simulator runs without UI automation (never
    /// synthetic taps): `-FRConnect <machine.tailnet.ts.net>` saves the
    /// machine if new and opens a session at launch; `-FRPort <port>` sets its
    /// port and `-FRControl` makes it control-capable (view-only otherwise).
    /// `-FRStatsLog` prints the session's content-free numbers once a second.
    func applyDebugLaunchArguments(_ arguments: [String]) {
        func value(_ flag: String) -> String? {
            guard let index = arguments.firstIndex(of: flag), index + 1 < arguments.count else { return nil }
            return arguments[index + 1]
        }
        if let name = value("-FRConnect") {
            let port = value("-FRPort").flatMap(UInt16.init) ?? UInt16(settings.defaultPort)
            let normalized = HostStore.normalize(name)
            let host = store.hosts.first { $0.name == normalized } ?? store.add(name, port: port)
            if var host {
                if host.port != port {
                    host.port = port
                    store.update(host)
                }
                connect(store.host(host.id) ?? host, control: arguments.contains("-FRControl"))
            } else {
                print("fr-debug: -FRConnect name refused")
            }
        }
        if arguments.contains("-FRStatsLog") {
            Task { @MainActor [weak self] in
                var last: FrStats?
                var ended = false
                while let self, !ended {
                    try? await Task.sleep(for: .seconds(1))
                    guard let session = self.session else { continue }
                    let s = session.stats()
                    let fps = last.map { Double(s.presented &- $0.presented) } ?? 0
                    let decoded = last.map { s.decoded &- $0.decoded } ?? s.decoded
                    let micros = last.map { s.decode_micros &- $0.decode_micros } ?? s.decode_micros
                    let decodeMs = decoded > 0 ? Double(micros) / Double(decoded) / 1000 : 0
                    print(String(format: "fr-stats phase=%@ fps=%.1f decode_ms=%.2f presented=%llu drawn_frame=%llu first_view_ms=%.0f hw=%d picture=%dx%d",
                                 "\(session.phase)", fps, decodeMs, s.presented, s.drawn_frame,
                                 Double(s.first_view_micros) / 1000, s.hardware,
                                 Int(session.picture.width), Int(session.picture.height)))
                    if case .ended(let end) = session.phase {
                        print("fr-ended reason=\(end) detail=\(session.detail)")
                        ended = true
                    }
                    last = s
                }
            }
        }
    }
}
#endif
