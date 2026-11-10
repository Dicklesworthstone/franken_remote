import Foundation

/// A machine the user added by its MagicDNS name.
struct SavedHost: Codable, Identifiable, Equatable, Hashable {
    var id: UUID
    /// `<machine>.<tailnet>.ts.net`, lowercase.
    var name: String
    var port: UInt16
    /// Optional label shown instead of the machine name.
    var nickname: String?
    /// The addresses this name resolved to when first trusted
    /// (`fr-pin-v1 ...`); a later resolution elsewhere is refused.
    var pin: String?
    var lastConnected: Date?
    /// Dial the pin's addresses instead of resolving the name: a machine a
    /// host's tailnet directory listed (absent in hosts saved before the
    /// directory existed).
    var direct: Bool?

    /// A direct dial needs the pin it dials.
    var dialsDirectly: Bool { direct == true && pin != nil }

    /// The machine label of the name (`mac-studio` in
    /// `mac-studio.tail1234.ts.net`).
    var machine: String { String(name.split(separator: ".").first ?? Substring(name)) }
    var title: String { nickname?.isEmpty == false ? nickname! : machine }
    /// The tailnet part, for the subtitle.
    var tailnet: String {
        let labels = name.split(separator: ".")
        return labels.count == 4 ? String(labels[1]) : ""
    }
}

/// Why a name was not accepted.
enum HostNameProblem: Equatable {
    case empty
    case notTailnetName
    case duplicate
}

/// Saved hosts, persisted as JSON in user defaults. Holds no secrets: names,
/// ports and the address pin are what the user typed or what Tailscale's DNS
/// answered.
final class HostStore: ObservableObject {
    @Published private(set) var hosts: [SavedHost] = []
    private let defaults: UserDefaults
    private let key: String
    /// The name check (the Rust core's in the app; injectable for tests).
    private let isValidName: (String) -> Bool

    init(defaults: UserDefaults = .standard, key: String = "savedHosts.v1",
         isValidName: @escaping (String) -> Bool) {
        self.defaults = defaults
        self.key = key
        self.isValidName = isValidName
        if let data = defaults.data(forKey: key),
           let decoded = try? JSONDecoder().decode([SavedHost].self, from: data) {
            hosts = decoded.filter { isValidName($0.name) }
        }
    }

    /// Normalize what the user typed: trim, lowercase, drop a trailing dot
    /// and any scheme or port they pasted along with it.
    static func normalize(_ typed: String) -> String {
        var text = typed.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        for scheme in ["https://", "http://"] where text.hasPrefix(scheme) {
            text.removeFirst(scheme.count)
        }
        if let slash = text.firstIndex(of: "/") { text = String(text[..<slash]) }
        if let colon = text.firstIndex(of: ":") { text = String(text[..<colon]) }
        if text.hasSuffix(".") { text.removeLast() }
        return text
    }

    func problem(with typed: String, excluding id: UUID? = nil) -> HostNameProblem? {
        let name = Self.normalize(typed)
        if name.isEmpty { return .empty }
        if !isValidName(name) { return .notTailnetName }
        if hosts.contains(where: { $0.name == name && $0.id != id }) { return .duplicate }
        return nil
    }

    @discardableResult
    func add(_ typed: String, port: UInt16, nickname: String? = nil) -> SavedHost? {
        guard problem(with: typed) == nil, port != 0 else { return nil }
        let host = SavedHost(id: UUID(), name: Self.normalize(typed), port: port,
                             nickname: nickname?.isEmpty == true ? nil : nickname)
        hosts.append(host)
        save()
        return host
    }

    func update(_ host: SavedHost) {
        guard let index = hosts.firstIndex(where: { $0.id == host.id }),
              isValidName(host.name), host.port != 0 else { return }
        var updated = host
        // A changed name is a different machine: its old pin does not apply.
        if hosts[index].name != host.name { updated.pin = nil }
        hosts[index] = updated
        save()
    }

    func remove(_ id: UUID) {
        hosts.removeAll { $0.id == id }
        save()
    }

    /// Record the pin a session observed. Only a pin for this host's own
    /// name is stored, and only when none is stored yet: replacing a pin
    /// is the user's explicit `forgetPin`.
    func recordPin(_ pin: String, for id: UUID) {
        guard let index = hosts.firstIndex(where: { $0.id == id }) else { return }
        let fields = pin.split(separator: " ")
        guard fields.count >= 3, fields[0] == "fr-pin-v1", fields[1] == hosts[index].name else { return }
        if hosts[index].pin == nil {
            hosts[index].pin = pin
        }
        hosts[index].lastConnected = Date()
        save()
    }

    func markConnected(_ id: UUID) {
        guard let index = hosts.firstIndex(where: { $0.id == id }) else { return }
        hosts[index].lastConnected = Date()
        save()
    }

    func forgetPin(_ id: UUID) {
        guard let index = hosts.firstIndex(where: { $0.id == id }) else { return }
        hosts[index].pin = nil
        save()
    }

    func host(_ id: UUID) -> SavedHost? { hosts.first { $0.id == id } }

    private func save() {
        if let data = try? JSONEncoder().encode(hosts) {
            defaults.set(data, forKey: key)
        }
    }
}

/// A source of machines beyond the ones the user typed.
///
/// Tailscale's iOS app gives other apps no way to list the tailnet's
/// machines (no LocalAPI, no shared container). A FrankenRemote host can:
/// `frd run --directory` answers an admitted viewer with the host itself and
/// that viewer's own untagged machines, read from the host's Tailscale
/// (PROTOCOL_DIRECTORY.md). The app asks a machine the user already saved,
/// and never shows a made-up list.
protocol HostDirectory {
    func fetch(from host: SavedHost) async -> DirectoryResult
}

/// One machine a host listed.
struct DirectoryMachine: Identifiable, Equatable, Hashable, Decodable {
    var id: String { nodeID }
    /// `<machine>.<tailnet>.ts.net`.
    var name: String
    /// Tailscale's stable node ID.
    var nodeID: String
    /// Tailscale's OS label (`macOS`, `linux`, `windows`, `iOS`...); may be empty.
    var os: String
    var online: Bool
    /// The host that answered.
    var serving: Bool
    var ipv4: String?
    var ipv6: String?
    /// `fr-pin-v1 <name> <listed addresses>`; nil when the app cannot dial it
    /// by name.
    var pin: String?

    enum CodingKeys: String, CodingKey {
        case name, nodeID = "node_id", os, online, serving, ipv4, ipv6, pin
    }

    /// The machine label of the name.
    var machine: String { String(name.split(separator: ".").first ?? Substring(name)) }

    /// A host to open a session to: the listed addresses, dialed directly,
    /// with TLS still checked for exactly this name. Nil without a pin.
    func host(port: UInt16) -> SavedHost? {
        guard let pin, port != 0 else { return nil }
        return SavedHost(id: UUID(), name: name, port: port, nickname: nil, pin: pin,
                         lastConnected: nil, direct: true)
    }
}

/// Why a host did not list its machines.
enum DirectoryRefusal: Equatable {
    /// The host does not serve a directory (`frd run --directory` is off).
    case disabled
    /// The host could not read its Tailscale status just now.
    case unavailable
    /// The host's FrankenRemote predates the directory.
    case unsupported
    /// The host's policy did not admit this device.
    case denied
    case other(String)

    init(reason: String) {
        switch reason {
        case "directory_disabled": self = .disabled
        case "directory_unavailable": self = .unavailable
        case "required_capability", "unsupported_version", "unsupported_profile": self = .unsupported
        case "permission_denied", "tailnet_membership_unverifiable": self = .denied
        default: self = .other(reason)
        }
    }

    func message(via host: String) -> String {
        switch self {
        case .disabled:
            return "\(host) doesn't list machines. Start FrankenRemote there with --directory to see your other machines here."
        case .unavailable:
            return "\(host) couldn't read its Tailscale status just now. Pull to refresh."
        case .unsupported:
            return "\(host) runs a FrankenRemote that can't list machines yet. Update it to see your other machines here."
        case .denied:
            return "\(host) didn't admit this device, so it listed nothing."
        case .other:
            return "\(host) refused to list machines."
        }
    }
}

/// One directory fetch's outcome, decoded from the Rust core's JSON
/// (`fr_directory_fetch`). Nothing is filled in that the host did not send.
enum DirectoryResult: Equatable {
    case listed(via: String, machines: [DirectoryMachine], truncated: Bool)
    case refused(via: String, DirectoryRefusal)
    /// `FR_END_*` and the typed failure's name.
    case failed(via: String, end: UInt32, detail: String)

    var via: String {
        switch self {
        case .listed(let via, _, _), .refused(let via, _), .failed(let via, _, _): return via
        }
    }

    private struct Document: Decodable {
        var status: String
        var via: String
        var truncated: Bool?
        var machines: [DirectoryMachine]?
        var reason: String?
        var end: UInt32?
        var detail: String?
    }

    /// Nil for anything but a complete, known document.
    static func parse(_ json: String) -> DirectoryResult? {
        guard let data = json.data(using: .utf8),
              let doc = try? JSONDecoder().decode(Document.self, from: data) else { return nil }
        switch doc.status {
        case "ok":
            guard let machines = doc.machines, let truncated = doc.truncated else { return nil }
            return .listed(via: doc.via, machines: machines, truncated: truncated)
        case "refused":
            guard let reason = doc.reason else { return nil }
            return .refused(via: doc.via, DirectoryRefusal(reason: reason))
        case "failed":
            guard let end = doc.end else { return nil }
            return .failed(via: doc.via, end: end, detail: doc.detail ?? "")
        default:
            return nil
        }
    }
}
