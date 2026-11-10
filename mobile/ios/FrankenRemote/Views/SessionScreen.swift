import SwiftUI
import UIKit

/// The full-screen session: the host's picture edge to edge (content kept in
/// the safe area), a toolbar that fades away while you work, and the
/// connection's state when there is no picture to show.
struct SessionScreen: View {
    @ObservedObject var session: RemoteSession
    @EnvironmentObject private var model: AppModel
    @EnvironmentObject private var settings: AppSettings
    @Environment(\.scenePhase) private var scenePhase
    @State private var keyboard = false
    @State private var toolbarVisible = true
    @State private var zoomCommand: SessionSurface.ZoomCommand?
    @State private var hideTask: Task<Void, Never>?
    @State private var stats = SessionStats()

    var body: some View {
        ZStack(alignment: .top) {
            SessionSurface(session: session, mode: settings.touchMode, settings: settings,
                           keyboard: $keyboard, zoom: $zoomCommand,
                           visible: scenePhase == .active && !session.phase.isEnded)
                .ignoresSafeArea()
            if !session.phase.isViewing {
                StatusCard(session: session, retry: retry, close: close)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            if toolbarVisible || !session.phase.isViewing {
                SessionToolbar(session: session, keyboard: $keyboard, zoom: $zoomCommand, close: close)
                    .transition(.move(edge: .top).combined(with: .opacity))
            } else {
                Button {
                    showToolbar()
                } label: {
                    Capsule().fill(.white.opacity(0.55)).frame(width: 44, height: 5)
                        .padding(.vertical, 10).padding(.horizontal, 24)
                        .contentShape(Rectangle())
                }
                .accessibilityLabel("Show toolbar")
            }
            if let notice = session.notice {
                Text(notice)
                    .font(.footnote)
                    .padding(.horizontal, 14).padding(.vertical, 10)
                    .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
                    .padding(.top, 64).padding(.horizontal, 16)
                    .transition(.opacity)
                    .task(id: notice) {
                        try? await Task.sleep(for: .seconds(4))
                        if session.notice == notice { session.notice = nil }
                    }
            }
            if settings.showStats, session.phase.isViewing {
                StatsOverlay(stats: stats)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottomLeading)
                    .padding(12)
                    .allowsHitTesting(false)
            }
        }
        .background(Color.black)
        .statusBarHidden(session.phase.isViewing && !toolbarVisible)
        .persistentSystemOverlays(session.phase.isViewing ? .hidden : .automatic)
        // In control, an edge swipe needs a second swipe to open Control
        // Center or go home: leaving the screen ends control (and the
        // session), so a stray swipe should not.
        .defersSystemGestures(on: session.phase == .controlled ? .all : [])
        .animation(.easeInOut(duration: 0.2), value: toolbarVisible)
        .onChange(of: session.phase, initial: true) { _, phase in
            if phase.isViewing { scheduleHide() } else { showToolbar() }
            // A session on screen keeps the device awake.
            UIApplication.shared.isIdleTimerDisabled = phase.isViewing
        }
        .onDisappear { UIApplication.shared.isIdleTimerDisabled = false }
        .task(id: settings.showStats) {
            guard settings.showStats else { return }
            while !Task.isCancelled {
                stats.update(session.stats())
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    private func showToolbar() {
        toolbarVisible = true
        scheduleHide()
    }

    private func scheduleHide() {
        hideTask?.cancel()
        hideTask = Task { @MainActor in
            try? await Task.sleep(for: .seconds(3))
            if !Task.isCancelled, session.phase.isViewing, !keyboard { toolbarVisible = false }
        }
    }

    private func retry() {
        let host = model.store.host(session.host.id) ?? session.host
        model.connect(host, control: session.controlCapable)
    }

    private func close() {
        model.disconnect()
    }
}

/// Connection progress and end reasons.
private struct StatusCard: View {
    @ObservedObject var session: RemoteSession
    let retry: () -> Void
    let close: () -> Void
    @EnvironmentObject private var model: AppModel
    @State private var showDetail = false

    var body: some View {
        VStack(spacing: 14) {
            switch session.phase {
            case .ended(let end):
                Image(systemName: end == .closedByYou ? "checkmark.circle" : "exclamationmark.triangle")
                    .font(.largeTitle)
                    .foregroundStyle(end == .closedByYou ? Color.secondary : Color.orange)
                Text(end.title).font(.headline)
                if !end.message.isEmpty {
                    Text(end.message).font(.subheadline).foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                }
                HStack {
                    if end.canRetry {
                        Button("Try again", action: retry).buttonStyle(.borderedProminent)
                    }
                    if end == .hostRefused, session.controlCapable {
                        Button("View only") {
                            model.connect(model.store.host(session.host.id) ?? session.host, control: false)
                        }
                        .buttonStyle(.bordered)
                    }
                    Button("Close", action: close).buttonStyle(.bordered)
                }
                if !session.detail.isEmpty {
                    DisclosureGroup("Details", isExpanded: $showDetail) {
                        Text(session.detail).font(.caption.monospaced()).textSelection(.enabled)
                    }
                    .font(.caption)
                    .frame(maxWidth: 320)
                }
            default:
                ProgressView().controlSize(.large).tint(.white)
                Text(progressTitle).font(.headline)
                Text(session.host.name).font(.caption).foregroundStyle(.secondary)
                if session.phase == .awaitingApproval {
                    Text("Approve this connection on the host's screen.")
                        .font(.subheadline).foregroundStyle(.secondary)
                }
            }
        }
        .padding(24)
        .frame(maxWidth: 420)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 20))
        .padding(24)
        .environment(\.colorScheme, .dark)
    }

    private var progressTitle: String {
        switch session.phase {
        case .connecting(let attempt): return attempt > 1 ? "Connecting (try \(attempt))…" : "Connecting…"
        case .authenticated: return "Opening the screen…"
        case .awaitingApproval: return "Waiting for approval"
        case .reconnecting: return "Reconnecting…"
        case .closing: return "Closing…"
        default: return "Connecting…"
        }
    }
}

/// Content-free numbers, refreshed once a second.
struct SessionStats: Equatable {
    var fps: Double = 0
    var decodeMilliseconds: Double = 0
    var firstViewMilliseconds: Double = 0
    var hardware: Int32 = -1
    private var lastPresented: UInt64 = 0
    private var lastDecoded: UInt64 = 0
    private var lastDecodeMicros: UInt64 = 0
    private var lastTime: Date?

    mutating func update(_ s: FrStats, now: Date = Date()) {
        if let lastTime {
            let seconds = now.timeIntervalSince(lastTime)
            if seconds > 0 {
                fps = Double(s.presented &- lastPresented) / seconds
            }
            let decoded = s.decoded &- lastDecoded
            if decoded > 0 {
                decodeMilliseconds = Double(s.decode_micros &- lastDecodeMicros) / Double(decoded) / 1000
            }
        }
        lastTime = now
        lastPresented = s.presented
        lastDecoded = s.decoded
        lastDecodeMicros = s.decode_micros
        firstViewMilliseconds = Double(s.first_view_micros) / 1000
        hardware = s.hardware
    }
}

private struct StatsOverlay: View {
    let stats: SessionStats
    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(String(format: "%.0f fps", stats.fps))
            Text(String(format: "decode %.1f ms", stats.decodeMilliseconds))
            Text(String(format: "first picture %.0f ms", stats.firstViewMilliseconds))
            Text(stats.hardware == 1 ? "hardware decode" : stats.hardware == 0 ? "software decode" : "decoder: unknown")
        }
        .font(.caption2.monospacedDigit())
        .foregroundStyle(.white)
        .padding(8)
        .background(.black.opacity(0.55), in: RoundedRectangle(cornerRadius: 8))
    }
}
