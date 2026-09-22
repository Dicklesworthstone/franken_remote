//
// SavedHost.swift — Saved Workstation Host Model
//

import Foundation

public struct SavedHost: Identifiable, Codable, Equatable, Hashable, Sendable {
    public let id: UUID
    public var name: String
    public var address: String
    public var port: UInt16
    public var tailnetDeviceName: String?
    public var lastConnected: Date?

    public init(
        id: UUID = UUID(),
        name: String,
        address: String,
        port: UInt16 = 443,
        tailnetDeviceName: String? = nil,
        lastConnected: Date? = nil
    ) {
        self.id = id
        self.name = name
        self.address = address
        self.port = port
        self.tailnetDeviceName = tailnetDeviceName
        self.lastConnected = lastConnected
    }

    public var displayEndpoint: String {
        if port == 443 {
            return address
        }
        return "\(address):\(port)"
    }
}
