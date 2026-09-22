//
// TouchMode.swift — Input Interaction Mode (Direct Touch vs Trackpad)
//
// Direct Touch Mode:
// - Direct 1:1 mapping from screen touch point to remote desktop coordinates.
// - Tap = Left click at tap location.
// - Two-finger tap = Right click at tap location.
// - Two-finger drag = Desktop scroll.
//
// Trackpad Mode:
// - Relative cursor movement with acceleration.
// - Virtual cursor indicator rendered on screen.
// - Tap anywhere = Left click at current virtual cursor position.
// - Two-finger tap = Right click at current virtual cursor position.
// - Two-finger drag = Scroll.
//

import Foundation

public enum TouchMode: String, CaseIterable, Identifiable, Codable, Sendable {
    case directTouch = "Direct Touch"
    case trackpad = "Trackpad"

    public var id: String { rawValue }

    public var systemImage: String {
        switch self {
        case .directTouch: return "hand.tap"
        case .trackpad: return "cursorarrow.rays"
        }
    }

    public var description: String {
        switch self {
        case .directTouch:
            return "Tap directly on screen elements to interact at that location."
        case .trackpad:
            return "Drag across the screen to move the cursor; tap anywhere to click."
        }
    }
}
