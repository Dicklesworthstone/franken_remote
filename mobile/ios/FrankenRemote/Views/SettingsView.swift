import SwiftUI

struct SettingsView: View {
    @EnvironmentObject private var settings: AppSettings
    @Environment(\.dismiss) private var dismiss
    @State private var port = ""

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Picker("Touch", selection: $settings.touchMode) {
                        ForEach(TouchMode.allCases) { mode in Text(mode.title).tag(mode) }
                    }
                    .pickerStyle(.segmented)
                    LabeledContent("Pointer speed") {
                        Slider(value: $settings.pointerSpeed, in: 0.5...2)
                            .frame(maxWidth: 180)
                    }
                    LabeledContent("Scroll speed") {
                        Slider(value: $settings.scrollSpeed, in: 0.5...3)
                            .frame(maxWidth: 180)
                    }
                    Toggle("Natural scrolling", isOn: $settings.naturalScrolling)
                    Toggle("Haptic feedback", isOn: $settings.haptics)
                } header: {
                    Text("Touch")
                } footer: {
                    Text(touchHelp)
                }
                Section {
                    Toggle("Ask for control on connect", isOn: $settings.controlOnConnect)
                    TextField("Default port", text: $port)
                        .keyboardType(.numberPad)
                        .onChange(of: port) { _, value in
                            if let number = Int(value), (1...65535).contains(number) {
                                settings.defaultPort = number
                            }
                        }
                } header: {
                    Text("Connecting")
                } footer: {
                    Text("Control is always asked for only once the host's screen is visible here, and the host may still decline. Leaving the app ends control.")
                }
                Section {
                    Toggle("Show session statistics", isOn: $settings.showStats)
                }
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
            .onAppear { port = String(settings.defaultPort) }
        }
    }

    private var touchHelp: String {
        switch settings.touchMode {
        case .trackpad:
            return "Trackpad: drag to move the pointer, tap to click, two-finger tap to right-click, touch and hold then drag to drag, two fingers to scroll, pinch to zoom."
        case .direct:
            return "Direct touch: tap where you want to click, drag to drag, touch and hold to right-click, two fingers to scroll, pinch to zoom."
        }
    }
}
