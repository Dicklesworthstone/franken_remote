import Combine
import Network
import SwiftUI
import UIKit

/// App-wide state: saved hosts, settings, the one live session, the network
/// path, and the reconnect offer after the app returns to the foreground.
@MainActor
final class AppModel: ObservableObject {
    let store: HostStore
    let settings: AppSettings
    /// Lists the user's machines through a saved FrankenRemote host
    /// (`frd run --directory`); see `HostDirectory`.
    let directory: HostDirectory
    /// The newest directory answer (nil before the first, or with no saved
    /// machine to ask).
    @Published private(set) var listing: DirectoryResult?
    /// The saved machine being asked right now.
    @Published private(set) var listingFrom: String?
    private var listingTask: Task<Void, Never>?
    /// A session to open: the host and whether it is control-capable.
    struct SessionStart: Equatable {
        var host: SavedHost
        var control: Bool
    }
    /// A start waiting for this app's directory fetch to the same machine to
    /// finish (`connect`), shown on the home screen while it waits.
    @Published private(set) var starting: SessionStart?
    private var connectTask: Task<Void, Never>?

    @Published private(set) var session: RemoteSession?
    /// A session the app closed when it went to the background (or a start
    /// it dropped), to offer again as it was (view-only stays view-only).
    @Published var reconnectOffer: SessionStart?
    @Published var startError: String?
    @Published private(set) var online = true
    @Published private(set) var pathDescription = ""

    private let monitor = NWPathMonitor()
    private var subscriptions: Set<AnyCancellable> = []
    private var lastPath: String?

    init(store: HostStore, settings: AppSettings, directory: HostDirectory = TailnetDirectory()) {
        self.store = store
        self.settings = settings
        self.directory = directory
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
    ///
    /// frd serves one connection at a time and rebinds its socket between
    /// them, so a session dialed while this app's directory fetch to the
    /// same machine is still open loses its first handshake packets and
    /// stalls about five seconds (seen live: two flows at launch, the
    /// session's handshake timed out on the host). Such a start waits for
    /// the fetch (one short exchange, at most three seconds) and the host's
    /// cooldown first.
    func connect(_ host: SavedHost, control: Bool) {
        reconnectOffer = nil
        startError = nil
        session?.close()
        session = nil
        subscriptions.removeAll()
        cancelStart()
        guard listingFrom == host.name else {
            start(host, control: control)
            return
        }
        starting = SessionStart(host: host, control: control)
        connectTask = Task { @MainActor [weak self] in
            let deadline = ContinuousClock.now + .seconds(3)
            while self?.listingFrom == host.name, ContinuousClock.now < deadline {
                try? await Task.sleep(for: .milliseconds(50))
                if Task.isCancelled { return }
            }
            // frd's cooldown before it listens again (250 ms).
            try? await Task.sleep(for: .milliseconds(400))
            guard let self, !Task.isCancelled else { return }
            self.connectTask = nil
            self.starting = nil
            self.start(host, control: control)
        }
    }

    /// Drop a start that is still waiting.
    private func cancelStart() {
        connectTask?.cancel()
        connectTask = nil
        starting = nil
    }

    /// This device's screen in its own pixels, longer side first (a desktop
    /// is landscape): the largest picture it shows 1:1. A Mac host then
    /// sends pictures that fit instead of its full 5K. 0, 0 when unknown.
    private static func screenPixels() -> (width: UInt32, height: UInt32) {
        let screen = UIApplication.shared.connectedScenes
            .compactMap { ($0 as? UIWindowScene)?.screen }
            .first
        guard let bounds = screen?.nativeBounds,
              min(bounds.width, bounds.height) >= 64 else { return (0, 0) }
        return (UInt32(max(bounds.width, bounds.height)),
                UInt32(min(bounds.width, bounds.height)))
    }

    private func start(_ host: SavedHost, control: Bool) {
        do {
            let next = try RemoteSession(host: host, control: control,
                                         requestControl: control && settings.controlOnConnect,
                                         screen: Self.screenPixels())
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

    /// Saved machines to ask for the directory, most recently used first: at
    /// most three, never a machine that itself came from a directory.
    nonisolated static func directoryCandidates(_ hosts: [SavedHost]) -> [SavedHost] {
        Array(hosts.filter { $0.direct != true }
            .sorted { ($0.lastConnected ?? .distantPast, $1.name) > ($1.lastConnected ?? .distantPast, $0.name) }
            .prefix(3))
    }

    /// Ask the saved machines in turn until one lists. With none listing,
    /// the most recent machine's answer stays (its refusal or failure).
    /// Never while a session is starting or open: the list is not on screen
    /// then, and a fetch would compete with the session for its host.
    func refreshDirectory() async {
        if sessionBusy { return }
        listingTask?.cancel()
        let task = Task { @MainActor [weak self] in
            guard let self else { return }
            let candidates = Self.directoryCandidates(self.store.hosts)
            guard !candidates.isEmpty else {
                self.listing = nil
                self.listingFrom = nil
                return
            }
            var first: DirectoryResult?
            for host in candidates {
                // A session that started meanwhile has its host to itself.
                if Task.isCancelled || self.sessionBusy {
                    self.listingFrom = nil
                    return
                }
                self.listingFrom = host.name
                let result = await self.directory.fetch(from: host)
                if Task.isCancelled { return }
                if case .listed = result {
                    self.listing = result
                    self.listingFrom = nil
                    return
                }
                first = first ?? result
            }
            self.listing = first
            self.listingFrom = nil
        }
        listingTask = task
        await task.value
    }

    /// Reconnect after the app came back: the saved host as it is now (a pin
    /// recorded since), with the capability the closed session had.
    func reconnect(_ offer: SessionStart) {
        connect(store.host(offer.host.id) ?? offer.host, control: offer.control)
    }

    /// Open a session to a machine a host listed. A machine the user saved
    /// opens exactly as saved (its port, its pin, resolved by name); any
    /// other at its listed addresses on the app's default frd port.
    func connect(_ machine: DirectoryMachine, control: Bool) {
        if let saved = Self.savedMatch(for: machine, in: store.hosts) {
            connect(saved, control: control)
            return
        }
        let port = UInt16(exactly: settings.defaultPort).flatMap { $0 == 0 ? nil : $0 }
            ?? UInt16(AppSettings.frdDefaultPort)
        guard let host = machine.host(port: port) else {
            startError = "FrankenRemote can't open \(machine.machine): its name isn't a MagicDNS machine name."
            return
        }
        connect(host, control: control)
    }

    /// A session is starting or open.
    private var sessionBusy: Bool {
        starting != nil || session.map { !$0.phase.isEnded } == true
    }

    /// The saved host a listed machine is, by its exact name.
    nonisolated static func savedMatch(for machine: DirectoryMachine, in hosts: [SavedHost]) -> SavedHost? {
        hosts.first { $0.name == machine.name.lowercased() }
    }

    func disconnect() {
        cancelStart()
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
            // A start still waiting would open the session unseen: drop it too.
            if let starting { reconnectOffer = starting }
            cancelStart()
            if let session, !session.phase.isEnded {
                reconnectOffer = SessionStart(host: session.host, control: session.controlCapable)
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
        if let name = value("-FRDirectory") {
            // Save the machine if new, then ask it for the directory and
            // print a content-free summary (no names or addresses).
            let port = value("-FRPort").flatMap(UInt16.init) ?? UInt16(settings.defaultPort)
            let normalized = HostStore.normalize(name)
            if store.hosts.first(where: { $0.name == normalized }) == nil,
               store.add(name, port: port) == nil {
                print("fr-debug: -FRDirectory name refused")
            }
            Task { @MainActor [weak self] in
                guard let self else { return }
                await self.refreshDirectory()
                switch self.listing {
                case .listed(_, let machines, let truncated):
                    print("fr-directory status=ok machines=\(machines.count) online=\(machines.filter(\.online).count) with_ipv6=\(machines.filter { $0.ipv6 != nil }.count) dialable=\(machines.filter { $0.pin != nil }.count) truncated=\(truncated)")
                case .refused(_, let refusal):
                    print("fr-directory status=refused reason=\(refusal)")
                case .failed(_, let end, let detail):
                    print("fr-directory status=failed end=\(end) detail=\(detail)")
                case nil:
                    print("fr-directory status=none")
                }
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
