import SwiftUI

/// The home screen: the user's saved machines, add-by-name, and the
/// reconnect offer after the app comes back to the foreground.
struct MachinePickerView: View {
    @EnvironmentObject private var model: AppModel
    @EnvironmentObject private var store: HostStore
    @EnvironmentObject private var settings: AppSettings
    @State private var editing: HostEditor.Target?
    @State private var showSettings = false

    var body: some View {
        NavigationStack {
            List {
                if let offer = model.reconnectOffer {
                    Section {
                        Button {
                            model.connect(offer, control: true)
                        } label: {
                            Label("Reconnect to \(offer.title)", systemImage: "arrow.clockwise")
                        }
                        Button("Not now", role: .cancel) { model.reconnectOffer = nil }
                    } footer: {
                        Text("The session closed when FrankenRemote left the screen.")
                    }
                }
                if let error = model.startError {
                    Section {
                        Label(error, systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.orange)
                    }
                }
                if !model.online {
                    Section {
                        Label("This device is offline.", systemImage: "wifi.slash")
                            .foregroundStyle(.secondary)
                    }
                }
                Section {
                    ForEach(sortedHosts) { host in
                        HostRow(host: host) { control in
                            model.connect(host, control: control)
                        } edit: {
                            editing = .edit(host)
                        }
                    }
                    .onDelete { offsets in
                        let hosts = sortedHosts
                        for index in offsets { store.remove(hosts[index].id) }
                    }
                    Button {
                        editing = .add
                    } label: {
                        Label("Add a machine", systemImage: "plus.circle")
                    }
                } header: {
                    Text("Machines")
                } footer: {
                    Text("Add a machine by its Tailscale MagicDNS name, like mac-studio.tail1234.ts.net (Tailscale app → the machine → its name). This device must be connected to the same tailnet, and the machine must be running FrankenRemote.")
                }
                if !store.hosts.isEmpty {
                    DirectorySection(listing: model.listing, asking: model.listingFrom) { machine, control in
                        model.connect(machine, control: control)
                    }
                }
            }
            .refreshable { await model.refreshDirectory() }
            .task(id: store.hosts.map(\.name)) { await model.refreshDirectory() }
            .navigationTitle("FrankenRemote")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        showSettings = true
                    } label: {
                        Image(systemName: "gearshape")
                    }
                    .accessibilityLabel("Settings")
                }
            }
            .sheet(item: $editing) { target in
                HostEditor(target: target)
                    .environmentObject(store)
                    .environmentObject(settings)
            }
            .sheet(isPresented: $showSettings) {
                SettingsView().environmentObject(settings)
            }
            .overlay {
                if store.hosts.isEmpty {
                    ContentUnavailableView {
                        Label("No machines yet", systemImage: "desktopcomputer")
                    } description: {
                        Text("Add a machine by its MagicDNS name to view and control it.")
                    } actions: {
                        Button("Add a machine") { editing = .add }
                            .buttonStyle(.borderedProminent)
                    }
                }
            }
        }
    }

    private var sortedHosts: [SavedHost] {
        store.hosts.sorted {
            ($0.lastConnected ?? .distantPast, $1.title) > ($1.lastConnected ?? .distantPast, $0.title)
        }
    }
}

/// The machines a saved host listed from its Tailscale (`frd run
/// --directory`): only what it sent, its refusal, or why it was unreachable.
private struct DirectorySection: View {
    let listing: DirectoryResult?
    let asking: String?
    let connect: (DirectoryMachine, Bool) -> Void

    var body: some View {
        Section {
            if let asking, listing == nil {
                HStack(spacing: 10) {
                    ProgressView()
                    Text("Asking \(label(asking)) for your machines…")
                        .foregroundStyle(.secondary)
                }
            }
            switch listing {
            case .listed(_, let machines, _):
                ForEach(machines) { machine in
                    DirectoryRow(machine: machine, connect: connect)
                }
            case .refused(let via, let refusal):
                Label(refusal.message(via: label(via)), systemImage: "list.bullet.rectangle")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            case .failed(let via, let end, _):
                Label("Couldn't ask \(label(via)) for your machines: \(SessionEnd(code: end).title.lowercased()).",
                      systemImage: "exclamationmark.triangle")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            case nil:
                EmptyView()
            }
        } header: {
            if case .listed(let via, _, _) = listing {
                Text("Machines on your tailnet (via \(label(via)))")
            } else {
                Text("Machines on your tailnet")
            }
        } footer: {
            if case .listed(_, _, let truncated) = listing {
                Text(truncated
                     ? "Listed by that machine from its Tailscale: your own machines only. Some could not be listed."
                     : "Listed by that machine from its Tailscale: your own machines only.")
            } else if asking == nil, listing == nil {
                Text("A saved machine running FrankenRemote with --directory lists the rest of your machines here.")
            }
        }
    }

    private func label(_ name: String) -> String {
        String(name.split(separator: ".").first ?? Substring(name))
    }
}

private struct DirectoryRow: View {
    let machine: DirectoryMachine
    let connect: (DirectoryMachine, Bool) -> Void

    var body: some View {
        Button {
            connect(machine, true)
        } label: {
            HStack(spacing: 12) {
                Image(systemName: symbol)
                    .font(.title2)
                    .foregroundStyle(machine.online ? AnyShapeStyle(.tint) : AnyShapeStyle(.secondary))
                    .frame(width: 32)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(machine.machine).font(.headline)
                        if machine.serving {
                            Text("this host")
                                .font(.caption2)
                                .padding(.horizontal, 5)
                                .padding(.vertical, 1)
                                .background(.quaternary, in: Capsule())
                        }
                    }
                    Text(details).font(.caption).foregroundStyle(.secondary)
                        .lineLimit(1).truncationMode(.middle)
                    if let addresses {
                        Text(addresses).font(.caption2.monospaced()).foregroundStyle(.secondary)
                            .lineLimit(1).truncationMode(.middle)
                    }
                }
                Spacer()
                Circle()
                    .fill(machine.online ? Color.green : Color.secondary.opacity(0.4))
                    .frame(width: 8, height: 8)
                    .accessibilityLabel(machine.online ? "Online" : "Offline")
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(machine.pin == nil)
        .contextMenu {
            Button { connect(machine, true) } label: { Label("View and control", systemImage: "cursorarrow.rays") }
            Button { connect(machine, false) } label: { Label("View only", systemImage: "eye") }
        }
    }

    private var symbol: String {
        switch machine.os.lowercased() {
        case "ios": return "iphone"
        case "android": return "smartphone"
        case "macos": return "desktopcomputer"
        case "windows", "linux": return "pc"
        default: return "server.rack"
        }
    }

    private var details: String {
        let state = machine.online ? "online" : "offline"
        return machine.os.isEmpty ? "\(machine.name) · \(state)" : "\(machine.os) · \(state)"
    }

    private var addresses: String? {
        let listed = [machine.ipv4, machine.ipv6].compactMap { $0 }
        return listed.isEmpty ? nil : listed.joined(separator: "  ")
    }
}

private struct HostRow: View {
    let host: SavedHost
    let connect: (Bool) -> Void
    let edit: () -> Void

    var body: some View {
        Button {
            connect(true)
        } label: {
            HStack(spacing: 12) {
                Image(systemName: "desktopcomputer")
                    .font(.title2)
                    .foregroundStyle(.tint)
                    .frame(width: 32)
                VStack(alignment: .leading, spacing: 2) {
                    Text(host.title).font(.headline)
                    Text(host.name).font(.caption).foregroundStyle(.secondary)
                        .lineLimit(1).truncationMode(.middle)
                }
                Spacer()
                if host.pin != nil {
                    Image(systemName: "lock.fill")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .accessibilityLabel("Address pinned")
                }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .contextMenu {
            Button { connect(true) } label: { Label("View and control", systemImage: "cursorarrow.rays") }
            Button { connect(false) } label: { Label("View only", systemImage: "eye") }
            Button { edit() } label: { Label("Edit", systemImage: "pencil") }
        }
        .swipeActions(edge: .leading) {
            Button { edit() } label: { Label("Edit", systemImage: "pencil") }.tint(.blue)
        }
    }
}

/// Add or edit a machine.
struct HostEditor: View {
    enum Target: Identifiable {
        case add
        case edit(SavedHost)
        var id: String {
            switch self {
            case .add: return "add"
            case .edit(let host): return host.id.uuidString
            }
        }
    }

    let target: Target
    @EnvironmentObject private var store: HostStore
    @EnvironmentObject private var settings: AppSettings
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var nickname = ""
    @State private var port = ""
    @State private var loaded = false

    private var existing: SavedHost? {
        if case .edit(let host) = target { return store.host(host.id) }
        return nil
    }

    private var problem: HostNameProblem? { store.problem(with: name, excluding: existing?.id) }
    private var portValue: UInt16? { UInt16(port).flatMap { $0 == 0 ? nil : $0 } }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("machine.tailnet.ts.net", text: $name)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                        .textContentType(.URL)
                } header: {
                    Text("MagicDNS name")
                } footer: {
                    switch problem {
                    case .some(.notTailnetName) where !name.isEmpty:
                        Text("Use the full name ending in .ts.net, exactly as the Tailscale app shows it.")
                            .foregroundStyle(.orange)
                    case .some(.duplicate):
                        Text("This machine is already saved.").foregroundStyle(.orange)
                    default:
                        Text("FrankenRemote checks the machine's certificate for exactly this name.")
                    }
                }
                Section("Label (optional)") {
                    TextField("Studio Mac", text: $nickname)
                }
                Section {
                    TextField("8443", text: $port).keyboardType(.numberPad)
                } header: {
                    Text("Port")
                } footer: {
                    Text("The port FrankenRemote listens on there (frd's default is 8443).")
                }
                if let host = existing, host.pin != nil {
                    Section {
                        Button("Forget the saved address pin", role: .destructive) {
                            store.forgetPin(host.id)
                        }
                    } footer: {
                        Text("The pin records the tailnet addresses this name had when you first connected. Forget it only if you re-created this machine in Tailscale.")
                    }
                }
            }
            .navigationTitle(existing == nil ? "Add a machine" : "Edit machine")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") { save() }.disabled(problem != nil || portValue == nil)
                }
            }
            .onAppear(perform: load)
        }
    }

    private func load() {
        guard !loaded else { return }
        loaded = true
        if let host = existing {
            name = host.name
            nickname = host.nickname ?? ""
            port = String(host.port)
        } else {
            port = String(settings.defaultPort)
        }
    }

    private func save() {
        guard let port = portValue else { return }
        let label = nickname.trimmingCharacters(in: .whitespaces)
        if var host = existing {
            host.name = HostStore.normalize(name)
            host.nickname = label.isEmpty ? nil : label
            host.port = port
            store.update(host)
        } else {
            store.add(name, port: port, nickname: label)
        }
        dismiss()
    }
}
