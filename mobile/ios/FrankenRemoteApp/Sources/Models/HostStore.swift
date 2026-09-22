//
// HostStore.swift — Saved Hosts and Tailnet Machine Discovery Store
//

import Foundation
import Combine

@MainActor
public final class HostStore: ObservableObject {
    @Published public private(set) var savedHosts: [SavedHost] = []
    @Published public private(set) var discoveredTailnetHosts: [SavedHost] = []

    private let storageKey = "com.frankenremote.saved_hosts"

    public init() {
        loadHosts()
    }

    public func saveHost(_ host: SavedHost) {
        if let idx = savedHosts.firstIndex(where: { $0.id == host.id }) {
            savedHosts[idx] = host
        } else {
            savedHosts.append(host)
        }
        persistHosts()
    }

    public func removeHost(at offsets: IndexSet) {
        savedHosts.remove(atOffsets: offsets)
        persistHosts()
    }

    public func updateLastConnected(id: UUID) {
        if let idx = savedHosts.firstIndex(where: { $0.id == id }) {
            savedHosts[idx].lastConnected = Date()
            persistHosts()
        }
    }

    public func updateDiscoveredHosts(_ hosts: [SavedHost]) {
        self.discoveredTailnetHosts = hosts
    }

    private func loadHosts() {
        guard let data = UserDefaults.standard.data(forKey: storageKey),
              let decoded = try? JSONDecoder().decode([SavedHost].self, from: data) else {
            self.savedHosts = []
            return
        }
        self.savedHosts = decoded
    }

    private func persistHosts() {
        if let data = try? JSONEncoder().encode(savedHosts) {
            UserDefaults.standard.set(data, forKey: storageKey)
        }
    }
}
