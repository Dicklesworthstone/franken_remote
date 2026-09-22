//
// VirtualCursorView.swift — Visibly Distinct Virtual Cursor for Trackpad Mode
//

import SwiftUI

public struct VirtualCursorView: View {
    public let position: CGPoint
    public let isPressed: Bool

    public init(position: CGPoint, isPressed: Bool = false) {
        self.position = position
        self.isPressed = isPressed
    }

    public var body: some View {
        ZStack {
            // Shadow / outline
            Circle()
                .stroke(Color.black.opacity(0.8), lineWidth: 2)
                .frame(width: 24, height: 24)

            // Inner circle
            Circle()
                .fill(isPressed ? Color.accentColor : Color.white)
                .frame(width: isPressed ? 18 : 20, height: isPressed ? 18 : 20)

            // Crosshair center dot
            Circle()
                .fill(isPressed ? Color.white : Color.black)
                .frame(width: 4, height: 4)
        }
        .position(position)
        .allowsHitTesting(false)
        .animation(.interactiveSpring(response: 0.15, dampingFraction: 0.8), value: isPressed)
    }
}
