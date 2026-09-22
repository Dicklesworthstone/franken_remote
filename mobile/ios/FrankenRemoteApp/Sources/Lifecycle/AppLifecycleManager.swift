//
// AppLifecycleManager.swift — Mobile Lifecycle & Authority Management
//
// Enforces Constitutional Invariants from Plan §16.2:
// 1. Backgrounding immediately releases remote input control and invalidates leases.
// 2. Resuming obtains a fresh lease and requests a recovery IDR frame.
// 3. Network transitions (Wi-Fi/Cellular) trigger route re-evaluation; never preserve stale leases.
// 4. Input queues and IME compositions are cancelled on backgrounding; never replayed.
//

import Foundation
import SwiftUI
import Network
import Combine

@MainActor
public final class AppLifecycleManager: ObservableObject {
    @Published public private(set) var isNetworkAvailable: Bool = true
    @Published public private(set) var isCellular: Bool = false
    @Published public private(set) var currentScenePhase: ScenePhase = .active

    private let pathMonitor = NWPathMonitor()
    private let monitorQueue = DispatchQueue(label: "com.frankenremote.network_monitor")

    public var onBackgroundEntered: (() -> Void)?
    public var onForegroundResumed: (() -> Void)?
    public var onNetworkRouteChanged: (() -> Void)?

    public init() {
        startNetworkMonitoring()
    }

    deinit {
        pathMonitor.cancel()
    }

    public func handleScenePhaseChange(_ newPhase: ScenePhase) {
        let oldPhase = currentScenePhase
        currentScenePhase = newPhase

        switch (oldPhase, newPhase) {
        case (_, .background):
            // Plan §16.2: Backgrounding client immediately releases control and drops queued input.
            onBackgroundEntered?()

        case (.background, .active):
            // Plan §16.2: Resuming obtains a fresh lease and recovery frame; never replays old inputs.
            onForegroundResumed?()

        default:
            break
        }
    }

    private func startNetworkMonitoring() {
        pathMonitor.pathUpdateHandler = { [weak self] path in
            Task { @MainActor [weak self] in
                guard let self = self else { return }
                let available = path.status == .satisfied
                let cellular = path.isExpensive || path.usesInterfaceType(.cellular)

                let routeChanged = (self.isCellular != cellular) || (self.isNetworkAvailable != available)

                self.isNetworkAvailable = available
                self.isCellular = cellular

                if routeChanged {
                    // Plan §16.2: Network transitions re-evaluate the route without preserving stale leases.
                    self.onNetworkRouteChanged?()
                }
            }
        }
        pathMonitor.start(queue: monitorQueue)
    }
}
