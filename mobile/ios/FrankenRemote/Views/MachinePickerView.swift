import SwiftUI

/// The home screen: the user's saved machines, add-by-name, and the
/// reconnect offer after the app comes back to the foreground.
struct MachinePickerView: View {
    @EnvironmentObject private var model: AppModel
    @EnvironmentObject private var store: HostStore
    @EnvironmentObject private var settings: AppSettings
    @State private var editing: HostEditor.Target?
    @State private var showSettings = false
    @State private var showReceivedFiles = false
    @State private var confirmReceiving = false
    @State private var receiveHost: SavedHost?
    @State private var receiveMachine: DirectoryMachine?
    @State private var pendingLink: MachineLink?
    @State private var linkError: String?

    var body: some View {
        NavigationStack {
            List {
                linkReview
                if let offer = model.reconnectOffer {
                    Section {
                        Button {
                            model.reconnect(offer)
                        } label: {
                            Label("Reconnect to \(offer.host.title)", systemImage: "arrow.clockwise")
                        }
                        Button("Not now", role: .cancel) { model.reconnectOffer = nil }
                    } footer: {
                        Text("The session closed when FrankenRemote left the screen.")
                    }
                }
                if let starting = model.starting {
                    Section {
                        HStack(spacing: 10) {
                            ProgressView()
                            Text("Connecting to \(starting.host.title)…")
                                .foregroundStyle(.secondary)
                        }
                        Button("Cancel", role: .cancel) { model.disconnect() }
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
                        } receiveFiles: {
                            receiveHost = host
                            receiveMachine = nil
                            confirmReceiving = true
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
                    } receiveFiles: { machine in
                        receiveHost = nil
                        receiveMachine = machine
                        confirmReceiving = true
                    }
                }
            }
            .refreshable { await model.refreshDirectory() }
            .task(id: store.hosts.map(\.name)) { await model.refreshDirectory() }
            .navigationTitle("FrankenRemote")
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button { showReceivedFiles = true } label: {
                        Image(systemName: "tray.and.arrow.down")
                    }
                    .accessibilityLabel("Received files")
                }
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
            .sheet(isPresented: $showReceivedFiles) { ReceivedFilesView() }
            .confirmationDialog("Allow files from \(receiveHost?.title ?? receiveMachine?.machine ?? "this host")?", isPresented: $confirmReceiving,
                                titleVisibility: .visible) {
                Button("Connect and allow receiving") {
                    if let host = receiveHost { model.connect(host, control: true, receiveFiles: true) }
                    else if let machine = receiveMachine { model.connect(machine, control: true, receiveFiles: true) }
                    receiveHost = nil
                    receiveMachine = nil
                }
                Button("Cancel", role: .cancel) {
                    receiveHost = nil
                    receiveMachine = nil
                }
            } message: {
                Text("Once you take control, this host can send selected files into FrankenRemote on this device: up to 256 MiB per file and 1 GiB for this session. Existing files are kept. Nothing opens automatically. This permission ends with the original connection; a reconnect needs a new choice.")
            }
            .overlay {
                if store.hosts.isEmpty && pendingLink == nil && linkError == nil {
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
        .onOpenURL { url in
            guard model.session == nil, model.starting == nil else { return }
            pendingLink = MachineLink(url)
            linkError = pendingLink == nil ? "This link needs a full MagicDNS machine name and an optional service port." : nil
        }
        .onChange(of: model.session != nil || model.starting != nil) { _, busy in
            if busy { dismissLink() }
        }
    }

    @ViewBuilder
    private var linkReview: some View {
        if let link = pendingLink {
            let saved = store.savedHost(for: link)
            Section {
                Text(verbatim: "\(link.name):\(link.port)")
                    .textSelection(.enabled)
                if let saved, saved.port != link.port {
                    Text(verbatim: "This machine is saved on port \(saved.port). To change its port, edit the saved machine.")
                } else {
                    Button(saved == nil ? "Save and view only" : "View only") {
                        guard model.session == nil, model.starting == nil else {
                            dismissLink()
                            return
                        }
                        dismissLink()
                        if let host = store.accept(link) {
                            model.connect(host, control: false)
                        } else {
                            linkError = "The saved machine changed. Open the link again to review it."
                        }
                    }
                }
                Button("Dismiss", role: .cancel) { dismissLink() }
            } header: {
                Text("Machine from link")
            }
        } else if let linkError {
            Section {
                Label(linkError, systemImage: "exclamationmark.triangle")
                Button("Dismiss", role: .cancel) { dismissLink() }
            } header: {
                Text("Machine link")
            }
        }
    }

    private func dismissLink() {
        pendingLink = nil
        linkError = nil
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
    let receiveFiles: (DirectoryMachine) -> Void

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
                    DirectoryRow(machine: machine, connect: connect, receiveFiles: receiveFiles)
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
    let receiveFiles: (DirectoryMachine) -> Void

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
            Button { receiveFiles(machine) } label: { Label("Connect and receive files…", systemImage: "tray.and.arrow.down") }
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
    let receiveFiles: () -> Void

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
            Button(action: receiveFiles) { Label("Connect and receive files…", systemImage: "tray.and.arrow.down") }
            if let url = host.shareURL {
                ShareLink(item: url) { Label("Share machine link", systemImage: "square.and.arrow.up") }
            }
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

    private var address: MachineAddressInput { MachineAddressInput(name, portField: port) }
    private var problem: HostNameProblem? { store.problem(with: address.name, excluding: existing?.id) }

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
                    Text("MagicDNS name or machine link")
                } footer: {
                    switch problem {
                    case .some(.notTailnetName) where !name.isEmpty:
                        Text("Enter a full MagicDNS name or paste a machine link from FrankenRemote.")
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
                    if address.portFromLink, let linkedPort = address.port {
                        LabeledContent("Port from link", value: String(linkedPort))
                    } else {
                        TextField("8443", text: $port).keyboardType(.numberPad)
                    }
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
                    Button("Save") { save() }.disabled(problem != nil || address.port == nil)
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
        let endpoint = address
        guard problem == nil, let port = endpoint.port else { return }
        let label = nickname.trimmingCharacters(in: .whitespaces)
        if var host = existing {
            host.name = endpoint.name
            host.nickname = label.isEmpty ? nil : label
            host.port = port
            store.update(host)
        } else {
            store.add(endpoint.name, port: port, nickname: label)
        }
        dismiss()
    }
}
