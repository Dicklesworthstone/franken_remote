//
// SettingsView.swift — Settings & Platform Permissions Explanations
//

import SwiftUI
import AVFoundation

public struct SettingsView: View {
    @Environment(\.dismiss) private var dismiss
    @State private var micPermissionStatus: String = "Unknown"

    public init() {}

    public var body: some View {
        NavigationStack {
            List {
                Section("Permissions") {
                    HStack {
                        Label("Microphone", systemImage: "mic")
                        Spacer()
                        Text(micPermissionStatus)
                            .foregroundStyle(.secondary)
                    }

                    HStack {
                        Label("Local Network", systemImage: "network")
                        Spacer()
                        Text("Tailnet Ingress")
                            .foregroundStyle(.secondary)
                    }
                }

                Section("Video Pipeline") {
                    LabeledContent("Video Codec", value: "HEVC (H.265)")
                    LabeledContent("Profile", value: "Main, 8-bit, 4:2:0")
                    LabeledContent("Hardware Decoder", value: "Apple VideoToolbox")
                    LabeledContent("Presentation", value: "Direct CAMetalLayer (Zero-Copy)")
                }

                Section("Audio Pipeline") {
                    LabeledContent("Audio Codec", value: "Opus (Low-Latency)")
                    LabeledContent("Sample Rate", value: "48 kHz Stereo")
                    LabeledContent("Uplink Mode", value: "Explicit Push-to-Talk")
                }

                Section("Network & Security") {
                    LabeledContent("Identity Authority", value: "Tailscale LocalAPI WhoIs")
                    LabeledContent("Transport", value: "Direct QUIC / Encrypted UDP")
                    LabeledContent("Input Expiry", value: "0.5s – 1.5s Leased Tickets")
                    LabeledContent("Clipboard", value: "Log-Scrubbed Memory Sync")
                }

                Section("About") {
                    LabeledContent("App Version", value: "1.0.0 (Phase 3 Native)")
                    LabeledContent("Core Engine", value: "FrankenRemote Rust Core (Asupersync)")
                }
            }
            .navigationTitle("Settings & Info")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") {
                        dismiss()
                    }
                }
            }
            .onAppear {
                checkMicPermission()
            }
        }
    }

    private func checkMicPermission() {
        switch AVAudioApplication.shared.recordPermission {
        case .granted: micPermissionStatus = "Granted"
        case .denied: micPermissionStatus = "Denied"
        case .undetermined: micPermissionStatus = "Not Determined"
        @unknown default: micPermissionStatus = "Unknown"
        }
    }
}
