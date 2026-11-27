import Combine
import Network
import SwiftUI
import UIKit

/// Interface changes are hints to retire the current connection. Only a new
/// Rust dial can validate the destination, tailnet route and TLS identity.
struct SessionNetworkPath: Equatable {
    enum Medium: Hashable { case wifi, cellular, ethernet, loopback, other }

    let online: Bool
    let media: Set<Medium>

    init(online: Bool, media: Set<Medium>) {
        self.online = online
        self.media = media
    }

    init(_ path: NWPath) {
        let types: [(NWInterface.InterfaceType, Medium)] = [
            (.wifi, .wifi), (.cellular, .cellular), (.wiredEthernet, .ethernet),
            (.loopback, .loopback), (.other, .other)
        ]
        self.init(online: path.status == .satisfied,
                  media: Set(types.compactMap { path.usesInterfaceType($0.0) ? $0.1 : nil }))
    }

    func interrupts(_ previous: Self?) -> Bool {
        if !online { return true }
        guard let previous else { return false }
        return self != previous
    }

    var description: String {
        guard online else { return "Offline" }
        if media.contains(.wifi) { return "Wi-Fi" }
        if media.contains(.cellular) { return "Cellular" }
        if media.contains(.ethernet) { return "Ethernet" }
        return "Online"
    }
}

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
        var receiveFiles = false
    }
    /// A start waiting for this app's directory fetch to the same machine to
    /// finish (`connect`), shown on the home screen while it waits.
    @Published private(set) var starting: SessionStart?
    private var connectTask: Task<Void, Never>?
    private var displaySwitchTask: Task<Void, Never>?
    @Published private(set) var switchingDisplay = false
    @Published private(set) var releasingControl = false
    @Published private(set) var displaySwitchError: String?

    @Published private(set) var session: RemoteSession?
    /// A closed receive owner stays here until its original disk cleanup is
    /// confirmed. Closing the screen never silently permits a replacement.
    private var retiredReceiver: RemoteSession?
    private var receiverCleanupTask: Task<Void, Never>?
    struct ReconnectOffer: Identifiable {
        let id = UUID()
        let host: SavedHost
        let controlCapable: Bool
        let original: RemoteSession?
        let reason: String

        func renewed(reason: String) -> Self {
            Self(host: host, controlCapable: controlCapable, original: original, reason: reason)
        }
    }

    @Published var reconnectOffer: ReconnectOffer?
    /// Retains the same closed owner if cleanup is interrupted a second time.
    private var resuming: ReconnectOffer?
    private var sceneAllowsResume = true
    @Published var startError: String?
    @Published private(set) var online = true
    @Published private(set) var pathDescription = ""

    private let monitor: NWPathMonitor?
    private var subscriptions: Set<AnyCancellable> = []
    private var lastPath: SessionNetworkPath?

    init(store: HostStore, settings: AppSettings, directory: HostDirectory = TailnetDirectory(),
         monitor: NWPathMonitor? = NWPathMonitor()) {
        self.monitor = monitor
        self.store = store
        self.settings = settings
        self.directory = directory
        monitor?.pathUpdateHandler = { [weak self] path in
            // The monitor's serial main queue preserves the order of a rapid
            // offline/online pair relative to cancellation and a new start.
            MainActor.assumeIsolated { self?.pathChanged(SessionNetworkPath(path)) }
        }
        monitor?.start(queue: .main)
    }

    deinit { monitor?.cancel() }

    convenience init() {
        self.init(store: HostStore(isValidName: { name in name.withCString { fr_host_name_valid($0) } }),
                  settings: AppSettings())
    }

    func pathChanged(_ path: SessionNetworkPath) {
        let interrupted = path.interrupts(lastPath)
        lastPath = path
        online = path.online
        pathDescription = path.description
        if interrupted, starting != nil || session.map({ !$0.phase.isEnded }) == true || switchingDisplay {
            interruptViewing(reason: path.online ? "The device changed networks." : "The network connection was lost.")
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
    func connect(_ host: SavedHost, control: Bool, receiveFiles: Bool = false) {
        connect(host, control: control, receiveFiles: receiveFiles, after: nil)
    }

    private func connect(_ host: SavedHost, control: Bool, receiveFiles: Bool, after offer: ReconnectOffer?) {
        cancelDisplaySwitch()
        reconnectOffer = nil
        startError = nil
        session?.close()
        if let original = session { retireReceiver(original) }
        session = nil
        subscriptions.removeAll()
        cancelStart()
        guard !receiveFiles || control else {
            startError = "Receiving files requires a control-capable connection."
            return
        }
        guard online else {
            startError = "This device is offline. Connect to the network before opening a session."
            return
        }
        guard listingFrom == host.name || receiveFiles || retiredReceiver != nil || offer != nil else {
            start(host, control: control)
            return
        }
        starting = SessionStart(host: host, control: control, receiveFiles: receiveFiles)
        resuming = offer
        connectTask = Task { @MainActor [weak self] in
            if let original = offer?.original, !(await Self.confirmedClose(original)) {
                guard !Task.isCancelled else { return }
                self?.cancelStart()
                self?.startError = "The previous session did not confirm a clean close. Choose a machine to start again."
                return
            }
            guard !Task.isCancelled else { return }
            if let previous = self?.retiredReceiver {
                let deadline = ContinuousClock.now + .seconds(5)
                while !previous.downloadCleanupComplete, ContinuousClock.now < deadline {
                    previous.pump()
                    if previous.downloadCleanupComplete { break }
                    try? await Task.sleep(for: .milliseconds(50))
                    if Task.isCancelled { return }
                }
                guard let self, !Task.isCancelled else { return }
                guard previous.downloadCleanupComplete else {
                    self.cancelStart()
                    self.startError = "The previous file receiver has not confirmed cleanup. No new connection was opened."
                    return
                }
                self.retiredReceiver = nil
            }
            if self?.listingFrom == host.name {
                let deadline = ContinuousClock.now + .seconds(3)
                while self?.listingFrom == host.name, ContinuousClock.now < deadline {
                    try? await Task.sleep(for: .milliseconds(50))
                    if Task.isCancelled { return }
                }
                // frd's cooldown before it listens again (250 ms).
                try? await Task.sleep(for: .milliseconds(400))
            }
            var download: FileDownloadResource?
            if receiveFiles {
                let prepared = await Task.detached(priority: .userInitiated) {
                    Result { try FileDownloadResource.prepare() }
                }.value
                guard !Task.isCancelled else { return } // token drops, with no session started
                switch prepared {
                case .success(let owner): download = owner
                case .failure:
                    guard let self else { return }
                    self.starting = nil
                    self.connectTask = nil
                    self.startError = "A private receive folder could not be prepared. No connection was opened."
                    return
                }
            }
            guard let self, !Task.isCancelled else { return }
            self.connectTask = nil
            self.starting = nil
            self.resuming = nil
            self.start(host, control: control, requestControl: offer == nil ? nil : false,
                       download: download, clipboard: offer == nil)
        }
    }

    /// Drop a start that is still waiting.
    private func cancelStart() {
        connectTask?.cancel()
        connectTask = nil
        starting = nil
        resuming = nil
    }

    /// The closed screen is no longer pumping. Continue collecting that exact
    /// original receiver so completed files become available without requiring
    /// another connection attempt. Failed cleanup retains custody indefinitely.
    private func retireReceiver(_ original: RemoteSession) {
        guard original.receivesFiles, retiredReceiver !== original else { return }
        retiredReceiver = original
        receiverCleanupTask?.cancel()
        receiverCleanupTask = Task { @MainActor [weak self, original] in
            while !Task.isCancelled, self?.retiredReceiver === original {
                original.pump()
                if original.downloadCleanupComplete {
                    self?.retiredReceiver = nil
                    self?.receiverCleanupTask = nil
                    return
                }
                try? await Task.sleep(for: .milliseconds(250))
            }
        }
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

    private func start(_ host: SavedHost, control: Bool, requestControl: Bool? = nil,
                       download: FileDownloadResource? = nil, clipboard: Bool = true) {
        do {
            let next = try RemoteSession(host: host, control: control,
                                         requestControl: control && (requestControl ?? settings.controlOnConnect),
                                         screen: Self.screenPixels(), download: download, clipboard: clipboard)
            next.$observedPin
                .compactMap { $0 }
                .sink { [weak self] pin in
                    // The offered destination can outlive edits to its saved row.
                    guard let store = self?.store, let saved = store.host(host.id),
                          saved.name == host.name, saved.port == host.port,
                          saved.dialsDirectly == host.dialsDirectly,
                          saved.pin == host.pin || saved.pin == pin else { return }
                    store.recordPin(pin, for: host.id)
                }
                .store(in: &subscriptions)
            store.markConnected(host.id)
            session = next
        } catch RemoteSession.StartError.invalid {
            startError = "That machine name or its saved pin isn't valid. Edit the machine and try again."
        } catch {
            startError = "The app could not start a session right now."
        }
    }

    /// Releasing control uses the same original-session cleanup proof as a
    /// display change. The replacement observes and requires a new Control
    /// action; it never carries selected files or the auto-control preference.
    func releaseControl(_ original: RemoteSession) {
        guard session === original, original.phase == .controlled, !switchingDisplay else { return }
        changeDisplay(original)
        if switchingDisplay { releasingControl = true }
    }

    /// A display switch is a new observing session to the original pinned
    /// host, after the old service has actually cleaned up. No saved catalog
    /// or input intent survives; control remains available as a fresh action.
    func changeDisplay(_ original: RemoteSession) {
        guard session === original, original.phase.isViewing, !switchingDisplay else { return }
        guard let host = original.displaySwitchHost() else {
            original.notice = "The current machine's identity is unavailable. Disconnect before opening another viewing session."
            return
        }
        cancelStart()
        cancelDisplaySwitch()
        switchingDisplay = true
        original.notice = nil
        // Close first: hiding a still-controlled view is a separate failure,
        // not evidence that the user's orderly close completed.
        original.close()
        original.setVisible(false)
        displaySwitchTask = Task { @MainActor [weak self, original] in
            let clean = await Self.confirmedClose(original)
            guard !Task.isCancelled, let self, self.session === original, self.switchingDisplay else { return }
            self.displaySwitchTask = nil
            self.switchingDisplay = false
            self.releasingControl = false
            guard clean else {
                self.displaySwitchError = "The previous session did not confirm a clean close. No new viewing session was opened."
                return
            }
            self.subscriptions.removeAll()
            self.session = nil
            self.start(host, control: original.controlCapable, requestControl: false, clipboard: false)
        }
    }

    private static func confirmedClose(_ original: RemoteSession) async -> Bool {
        let clock = ContinuousClock()
        let began = clock.now
        var cleanup = DisplayReopen()
        while !Task.isCancelled {
            original.pump()
            switch cleanup.poll(status: original.reopenStatus, elapsed: began.duration(to: clock.now)) {
            case .ready: return true
            case .refused: return false
            case .waiting: try? await Task.sleep(for: .milliseconds(50))
            }
        }
        return false
    }

    private func cancelDisplaySwitch() {
        displaySwitchTask?.cancel()
        displaySwitchTask = nil
        switchingDisplay = false
        releasingControl = false
        displaySwitchError = nil
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

    /// A new authenticated observation of the original destination. An
    /// interrupted control-capable session needs a new physical Control tap.
    func reconnect(_ offer: ReconnectOffer) {
        guard reconnectOffer?.id == offer.id, online, sceneAllowsResume else { return }
        connect(offer.host, control: offer.controlCapable, receiveFiles: false, after: offer)
    }

    private func interruptViewing(reason: String) {
        cancelDisplaySwitch()
        if let previous = resuming {
            reconnectOffer = previous.renewed(reason: reason)
        } else if let starting {
            // The queued start has not opened a service. Drop all one-use choices.
            reconnectOffer = ReconnectOffer(host: starting.host, controlCapable: starting.control,
                                            original: nil, reason: reason)
        }
        cancelStart()
        guard let original = session else { return }
        if !original.phase.isEnded {
            let host = original.displaySwitchHost()
            original.close()
            original.setVisible(false)
            if let host {
                reconnectOffer = ReconnectOffer(host: host, controlCapable: original.controlCapable,
                                                original: original, reason: reason)
            } else {
                startError = reason + " Select the machine again to verify its identity."
            }
        }
        retireReceiver(original)
        session = nil
        subscriptions.removeAll()
    }

    /// Open a session to a machine a host listed. A machine the user saved
    /// opens exactly as saved (its port, its pin, resolved by name); any
    /// other at its listed addresses on the app's default frd port.
    func connect(_ machine: DirectoryMachine, control: Bool, receiveFiles: Bool = false) {
        if let saved = Self.savedMatch(for: machine, in: store.hosts) {
            connect(saved, control: control, receiveFiles: receiveFiles)
            return
        }
        let port = UInt16(exactly: settings.defaultPort).flatMap { $0 == 0 ? nil : $0 }
            ?? UInt16(AppSettings.frdDefaultPort)
        guard let host = machine.host(port: port) else {
            startError = "FrankenRemote can't open \(machine.machine): its name isn't a MagicDNS machine name."
            return
        }
        connect(host, control: control, receiveFiles: receiveFiles)
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
        cancelDisplaySwitch()
        cancelStart()
        session?.close()
        if let original = session { retireReceiver(original) }
        session = nil
        subscriptions.removeAll()
    }

    /// The scene moved between foreground and background.
    func scenePhaseChanged(_ phase: ScenePhase) {
        switch phase {
        case .background:
            sceneAllowsResume = false
            interruptViewing(reason: "FrankenRemote went into the background.")
        case .inactive:
            sceneAllowsResume = false
            if let offer = resuming {
                cancelStart()
                reconnectOffer = offer.renewed(reason: "FrankenRemote left the foreground.")
            }
            // A scheduled replacement must not open behind Control Center,
            // a notification, or another app, even if the scene stays loaded.
            cancelDisplaySwitch()
            // A prepared receive destination never starts a session behind a
            // permission sheet, Control Center or another foreground owner.
            if starting?.receiveFiles == true { cancelStart() }
        case .active:
            sceneAllowsResume = true
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
