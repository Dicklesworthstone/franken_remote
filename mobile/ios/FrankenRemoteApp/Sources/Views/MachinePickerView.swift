//
// MachinePickerView.swift — Tailnet Machine Picker & Connection Launcher
//

import SwiftUI
import FrankenRemoteKit

public struct MachinePickerView: View {
    @ObservedObject public var hostStore: HostStore
    @ObservedObject public var lifecycleManager: AppLifecycleManager

    public let onConnectToHost: (SavedHost, String) -> Void

    @State private var isAddHostPresented: Bool = false
    @State private var isSettingsPresented: Bool = false
    @State private var connectingHostId: UUID? = nil
    @State private var authTokenInput: String = ""
    @State private var hostPendingAuth: SavedHost? = nil

    public init(
        hostStore: HostStore,
        lifecycleManager: AppLifecycleManager,
        onConnectToHost: @escaping (SavedHost, String) -> Void
    ) {
        self.hostStore = hostStore
        self.lifecycleManager = lifecycleManager
        self.onConnectToHost = onConnectToHost
    }

    public var body: some View {
        NavigationStack {
            List {
                if !lifecycleManager.isNetworkAvailable {
                    Section {
                        Label("Network Offline", systemImage: "wifi.slash")
                            .foregroundStyle(.red)
                    }
                }

                // Saved Workstations Section
                Section("Saved Machines") {
                    if hostStore.savedHosts.isEmpty {
                        Text("No saved machines. Add your Tailscale workstation to connect.")
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                    } else {
                        ForEach(hostStore.savedHosts) { host in
                            hostRow(host)
                        }
                        .onDelete(perform: hostStore.removeHost)
                    }
                }

                // Discovered Tailnet Section
                if !hostStore.discoveredTailnetHosts.isEmpty {
                    Section("Tailnet Devices") {
                        ForEach(hostStore.discoveredTailnetHosts) { host in
                            hostRow(host)
                        }
                    }
                }
            }
            .navigationTitle("FrankenRemote")
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button {
                        isSettingsPresented = true
                    } label: {
                        Image(systemName: "gear")
                    }
                }
                ToolbarItem(placement: .primaryAction) {
                    Button {
                        isAddHostPresented = true
                    } label: {
                        Image(systemName: "plus")
                    }
                }
            }
            .sheet(isPresented: $isAddHostPresented) {
                AddHostSheet { newHost in
                    hostStore.saveHost(newHost)
                }
            }
            .sheet(isPresented: $isSettingsPresented) {
                SettingsView()
            }
            .alert(
                "Connect to \(hostPendingAuth?.name ?? "Host")",
                isPresented: Binding(
                    get: { hostPendingAuth != nil },
                    set: { if !$0 { hostPendingAuth = nil } }
                )
            ) {
                SecureField("Session Auth Token", text: $authTokenInput)
                Button("Connect") {
                    if let host = hostPendingAuth {
                        onConnectToHost(host, authTokenInput)
                        authTokenInput = ""
                        hostPendingAuth = nil
                    }
                }
                Button("Cancel", role: .cancel) {
                    authTokenInput = ""
                    hostPendingAuth = nil
                }
            } message: {
                Text("Enter the single-use bootstrap token provided by your host daemon.")
            }
        }
    }

    private func hostRow(_ host: SavedHost) -> some View {
        Button {
            hostPendingAuth = host
        } label: {
            HStack(spacing: 12) {
                Image(systemName: "display")
                    .font(.title2)
                    .foregroundStyle(.tint)
                    .frame(width: 32)

                VStack(alignment: .leading, spacing: 2) {
                    Text(host.name)
                        .font(.body.weight(.medium))
                        .foregroundStyle(.primary)

                    Text(host.displayEndpoint)
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                }

                Spacer()

                if let last = host.lastConnected {
                    Text(last, style: .relative)
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }

                Image(systemName: "chevron.right")
                    .font(.caption2.weight(.bold))
                    .foregroundStyle(.tertiary)
            }
            .padding(.vertical, 4)
        }
    }
}

/// Sheet for adding a new machine to the saved list.
struct AddHostSheet: View {
    @Environment(\.dismiss) private var dismiss

    @State private var name: String = ""
    @State private var address: String = ""
    @State private var portString: String = "443"

    let onSave: (SavedHost) -> Void

    var body: some View {
        NavigationStack {
            Form {
                Section("Host Details") {
                    TextField("Display Name (e.g. Mac Studio)", text: $name)
                    TextField("Tailnet Address (IP or .ts.net)", text: $address)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    TextField("Port", text: $portString)
                        .keyboardType(.numberPad)
                }
            }
            .navigationTitle("Add Machine")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        let port = UInt16(portString) ?? 443
                        let host = SavedHost(name: name.isEmpty ? address : name, address: address, port: port)
                        onSave(host)
                        dismiss()
                    }
                    .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
        }
    }
}
