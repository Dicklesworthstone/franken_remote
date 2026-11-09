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
/// machines (no LocalAPI, no shared container), so the app ships with no
/// directory and never shows a made-up list. A real directory (for example
/// the Tailscale API with a user-issued read-only token) can implement this
/// and appear in the picker.
protocol HostDirectory {
    var title: String { get }
    func machines() async throws -> [DirectoryMachine]
}

struct DirectoryMachine: Identifiable, Equatable {
    var id: String { name }
    /// `<machine>.<tailnet>.ts.net`.
    var name: String
    var online: Bool
}
