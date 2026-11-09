import SwiftUI

/// Close, the host and its state, control, keyboard, touch mode and zoom.
struct SessionToolbar: View {
    @ObservedObject var session: RemoteSession
    @Binding var keyboard: Bool
    @Binding var zoom: SessionSurface.ZoomCommand?
    let close: () -> Void
    @EnvironmentObject private var settings: AppSettings
    @State private var confirmRelease = false

    var body: some View {
        HStack(spacing: 10) {
            Button(action: close) {
                Image(systemName: "xmark")
            }
            .accessibilityLabel("Disconnect")
            VStack(alignment: .leading, spacing: 1) {
                Text(session.host.title).font(.subheadline.weight(.semibold)).lineLimit(1)
                Text(stateText).font(.caption2).foregroundStyle(.secondary)
            }
            Spacer(minLength: 4)
            if session.phase.isViewing {
                controlButton
                Button {
                    keyboard.toggle()
                } label: {
                    Image(systemName: keyboard ? "keyboard.fill" : "keyboard")
                }
                .disabled(session.phase != .controlled)
                .accessibilityLabel(keyboard ? "Hide keyboard" : "Show keyboard")
                Menu {
                    Picker("Touch", selection: $settings.touchMode) {
                        ForEach(TouchMode.allCases) { mode in Text(mode.title).tag(mode) }
                    }
                    Divider()
                    Button { zoom = .fit } label: { Label("Fit to screen", systemImage: "arrow.down.right.and.arrow.up.left") }
                    Button { zoom = .actualPixels } label: { Label("Actual pixels", systemImage: "1.magnifyingglass") }
                    Toggle("Statistics", isOn: $settings.showStats)
                } label: {
                    Image(systemName: settings.touchMode == .trackpad ? "rectangle.and.hand.point.up.left" : "hand.point.up.left")
                }
                .accessibilityLabel("View options")
            }
        }
        .font(.body.weight(.medium))
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .background(.ultraThinMaterial, in: Capsule())
        .padding(.horizontal, 12)
        .padding(.top, 4)
        .environment(\.colorScheme, .dark)
        .confirmationDialog("Give back control?", isPresented: $confirmRelease, titleVisibility: .visible) {
            Button("Give back control and disconnect", role: .destructive) { session.releaseControl() }
        } message: {
            Text("Ending control ends this session. Reconnect to view again.")
        }
    }

    @ViewBuilder private var controlButton: some View {
        switch session.phase {
        case .controlled:
            Button {
                confirmRelease = true
            } label: {
                Label("In control", systemImage: "cursorarrow.rays")
                    .labelStyle(.titleAndIcon)
                    .font(.caption.weight(.semibold))
            }
            .buttonStyle(.borderedProminent)
            .tint(.green)
        case .requestingControl:
            ProgressView().controlSize(.small)
        default:
            if session.controlCapable {
                Button {
                    session.requestControl()
                } label: {
                    Label("Control", systemImage: "cursorarrow")
                        .labelStyle(.titleAndIcon)
                        .font(.caption.weight(.semibold))
                }
                .buttonStyle(.bordered)
            }
        }
    }

    private var stateText: String {
        switch session.phase {
        case .viewing: return session.controlCapable ? "Viewing" : "Viewing only"
        case .requestingControl: return "Asking for control…"
        case .controlled: return "In control"
        case .connecting: return "Connecting…"
        case .authenticated: return "Opening…"
        case .awaitingApproval: return "Waiting for approval"
        case .reconnecting: return "Reconnecting…"
        case .closing: return "Closing…"
        case .ended(let end): return end.title
        }
    }
}

/// The UIKit surface inside SwiftUI.
struct SessionSurface: UIViewRepresentable {
    enum ZoomCommand: Equatable { case fit, actualPixels }

    let session: RemoteSession
    let mode: TouchMode
    let settings: AppSettings
    @Binding var keyboard: Bool
    @Binding var zoom: ZoomCommand?
    /// On screen in the active app: frames count as seen only then, and
    /// losing it ends control.
    let visible: Bool

    func makeUIView(context: Context) -> RemoteSurfaceView {
        let view = RemoteSurfaceView(frame: .zero)
        view.session = session
        view.onKeyboardHidden = { DispatchQueue.main.async { keyboard = false } }
        return view
    }

    func updateUIView(_ view: RemoteSurfaceView, context: Context) {
        if view.session !== session { view.session = session }
        view.interpreter.mode = mode
        view.interpreter.acceleration.speed = CGFloat(settings.pointerSpeed)
        view.interpreter.scroller.natural = settings.naturalScrolling
        view.interpreter.scroller.pointsPerLine = 18 / CGFloat(max(0.25, settings.scrollSpeed))
        view.hapticsEnabled = settings.haptics
        session.setVisible(visible)
        if !visible { view.suspend() }
        view.setSoftwareKeyboard(keyboard && visible && session.phase == .controlled)
        if let command = zoom {
            switch command {
            case .fit: view.zoomToFit()
            case .actualPixels: view.zoomToActualPixels()
            }
            DispatchQueue.main.async { zoom = nil }
        }
    }

    static func dismantleUIView(_ view: RemoteSurfaceView, coordinator: ()) {
        view.suspend()
        view.session?.setVisible(false)
    }
}
