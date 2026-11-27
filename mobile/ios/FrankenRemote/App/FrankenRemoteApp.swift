import SwiftUI

@main
struct FrankenRemoteApp: App {
    @StateObject private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            MachinePickerView()
                .environmentObject(model)
                .environmentObject(model.store)
                .environmentObject(model.settings)
                #if DEBUG
                .task { model.applyDebugLaunchArguments(ProcessInfo.processInfo.arguments) }
                #endif
                .fullScreenCover(item: Binding(
                    get: { model.session.map(ActiveSession.init) },
                    set: { [original = model.session] value in
                        if value == nil, let original { model.disconnect(original) }
                    })
                ) { active in
                    SessionScreen(session: active.session)
                        .id(active.session.handle)
                        .environmentObject(model)
                        .environmentObject(model.settings)
                }
        }
        .onChange(of: scenePhase) { _, phase in
            model.scenePhaseChanged(phase)
        }
    }
}

/// Identifies the presented session for `fullScreenCover`.
struct ActiveSession: Identifiable {
    let session: RemoteSession
    var id: ObjectIdentifier { ObjectIdentifier(session) }
}
