//
// ConnectionQualityBadge.swift — Real-Time Session Health & Quality HUD
//

import SwiftUI
import FrankenRemoteKit

public struct ConnectionQualityBadge: View {
    public let quality: ConnectionQuality
    public let isCellular: Bool

    public init(quality: ConnectionQuality, isCellular: Bool = false) {
        self.quality = quality
        self.isCellular = isCellular
    }

    public var body: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(statusColor)
                .frame(width: 8, height: 8)

            Text("\(quality.rttMs)ms")
                .font(.caption2.monospacedDigit().weight(.semibold))

            Text("•")
                .foregroundStyle(.secondary)

            Text("\(quality.fps) FPS")
                .font(.caption2.monospacedDigit())

            if quality.bitrateKbps > 0 {
                Text("•")
                    .foregroundStyle(.secondary)
                Text("\(quality.bitrateKbps / 1000)M")
                    .font(.caption2.monospacedDigit())
            }

            if isCellular {
                Image(systemName: "antenna.radiowaves.left.and.right")
                    .font(.caption2)
                    .foregroundStyle(.orange)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 4)
        .background(.ultraThinMaterial, in: Capsule())
        .overlay(
            Capsule()
                .strokeBorder(Color.white.opacity(0.15), lineWidth: 0.5)
        )
    }

    private var statusColor: Color {
        switch quality.qualityTier {
        case .excellent: return .green
        case .good: return .blue
        case .degraded: return .yellow
        case .poor: return .red
        }
    }
}
