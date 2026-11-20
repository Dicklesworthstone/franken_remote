import SwiftUI

/// This stays on screen even when the input toolbar fades. Its original
/// session's native gate expires if these actual visible callbacks stop.
struct ClipboardBar: View {
    @ObservedObject var session: RemoteSession
    let visible: Bool

    private var active: Bool {
        visible && session.phase.isViewing && session.clipboardStatus.enabled
    }

    var body: some View {
        if session.clipboardRequested {
            HStack(spacing: 10) {
                Image(systemName: "doc.on.clipboard")
                VStack(alignment: .leading, spacing: 2) {
                    Text(session.clipboardStatus.title(controlled: session.phase == .controlled,
                                                       ended: session.phase.isEnded))
                        .font(.caption.weight(.semibold))
                    if session.clipboardStatus.uncertain > 0 {
                        Text("An earlier copy to this device was not confirmed.")
                            .font(.caption2)
                    }
                }
                Spacer(minLength: 4)
                if session.clipboardStatus.enabled && !session.phase.isEnded {
                    Button("Turn off") { session.stopClipboard() }
                        .buttonStyle(.bordered)
                        .accessibilityLabel("Turn off text clipboard sharing")
                }
            }
            .padding(.horizontal, 12).padding(.vertical, 8)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
            .environment(\.colorScheme, .dark)
            .task(id: active) {
                while !Task.isCancelled {
                    session.clipboardPresence(active)
                    session.pollClipboard()
                    if !active || !session.clipboardStatus.enabled { return }
                    try? await Task.sleep(for: .milliseconds(50))
                }
            }
            .onDisappear { session.clipboardPresence(false) }
        }
    }
}
