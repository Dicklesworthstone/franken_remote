//
// App.swift — FrankenRemote iOS Application Entry Point
//
// Native SwiftUI application coordinating workstation discovery, connection
// lifecycle, and full-screen hardware-accelerated remote desktop sessions.
//

import SwiftUI
import FrankenRemoteKit

@main
struct FrankenRemoteApp: App {
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var hostStore = HostStore()
    @StateObject private var lifecycleManager = AppLifecycleManager()

    @State private var activeSessionController: FrankenSessionController?
    @State private var activeHost: SavedHost?
    @State private var connectionErrorMessage: String?

    var body: some Scene {
        WindowGroup {
            Group {
                if let controller = activeSessionController {
                    SessionViewerContainerView(
                        controller: controller,
                        lifecycleManager: lifecycleManager,
                        onExit: {
                            cleanupSession()
                        }
                    )
                } else {
                    MachinePickerView(
                        hostStore: hostStore,
                        lifecycleManager: lifecycleManager,
                        onConnectToHost: { host, token in
                            connectToHost(host, token: token)
                        }
                    )
                    .alert(
                        "Connection Error",
                        isPresented: Binding(
                            get: { connectionErrorMessage != nil },
                            set: { if !$0 { connectionErrorMessage = nil } }
                        )
                    ) {
                        Button("OK", role: .cancel) {
                            connectionErrorMessage = nil
                        }
                    } message: {
                        Text(connectionErrorMessage ?? "Failed to connect to host")
                    }
                }
            }
            .onChange(of: scenePhase) { newPhase in
                lifecycleManager.handleScenePhaseChange(newPhase)
            }
            .onAppear {
                setupLifecycleCallbacks()
            }
        }
    }

    private func connectToHost(_ host: SavedHost, token: String) {
        do {
            let controller = try FrankenSessionController(
                hostAddress: host.displayEndpoint,
                authToken: token
            )
            self.activeHost = host
            self.activeSessionController = controller
            hostStore.updateLastConnected(id: host.id)

            Task {
                do {
                    try await controller.connect()
                } catch {
                    Task { @MainActor in
                        self.connectionErrorMessage = error.localizedDescription
                        cleanupSession()
                    }
                }
            }
        } catch {
            self.connectionErrorMessage = error.localizedDescription
        }
    }

    private func cleanupSession() {
        if let controller = activeSessionController {
            controller.disconnect()
            controller.close()
        }
        activeSessionController = nil
        activeHost = nil
    }

    private func setupLifecycleCallbacks() {
        lifecycleManager.onBackgroundEntered = {
            // Plan §16.2: Backgrounding immediately releases remote control.
            if let controller = activeSessionController {
                controller.disconnect()
            }
        }

        lifecycleManager.onForegroundResumed = {
            // Plan §16.2: Resume reconnects with fresh lease and recovery frame.
            if let controller = activeSessionController {
                Task {
                    try? await controller.connect()
                }
            }
        }

        lifecycleManager.onNetworkRouteChanged = {
            // Plan §16.2: Network transitions re-evaluate route without preserving stale leases.
            if let controller = activeSessionController {
                controller.disconnect()
                Task {
                    try? await controller.connect()
                }
            }
        }
    }
}
