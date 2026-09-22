//
// SessionController.swift — FrankenRemote Swift Package Session Controller
//
// Wraps the fr_mobile C-ABI boundary in idiomatic Swift with async/await,
// @MainActor state observation, Sendable conformance, and generation safety.
//
// Constitutional Invariants (Plan §16.2 & ADR 0003):
// 1. All logic stays in Rust (fr-client); Swift only passes commands/callbacks.
// 2. Generation-checked opaque handles safely reject stale calls or double-free.
// 3. Zero-copy decoded video via direct CAMetalLayer attachment.
//

import Foundation
import QuartzCore
import Combine
import CFrankenRemote

/// Typed errors returned by FrankenRemote mobile operations.
public enum FrankenError: Int32, Error, CustomStringConvertible, Sendable {
    case invalidArgument = -1
    case staleHandle = -2
    case alreadyClosed = -3
    case connectionFailed = -4
    case timeout = -5
    case permissionDenied = -6
    case unsupported = -7
    case unknown = -99

    public init(code: Int32) {
        self = FrankenError(rawValue: code) ?? .unknown
    }

    public var description: String {
        switch self {
        case .invalidArgument: return "Invalid argument passed to session"
        case .staleHandle: return "Stale session handle: session generation invalid or expired"
        case .alreadyClosed: return "Session has already been closed"
        case .connectionFailed: return "Failed to establish or maintain host connection"
        case .timeout: return "Operation timed out"
        case .permissionDenied: return "Permission denied by host authority policy"
        case .unsupported: return "Operation unsupported on current host or device"
        case .unknown: return "Unknown FrankenRemote error"
        }
    }
}

/// Remote workstation session lifecycle state.
public enum SessionState: Equatable, Sendable {
    case disconnected
    case connecting
    case authenticating
    case connected
    case reconnecting
    case failed(String)

    init(cState: Int32, detail: String?) {
        switch cState {
        case 0: self = .disconnected
        case 1: self = .connecting
        case 2: self = .authenticating
        case 3: self = .connected
        case 4: self = .reconnecting
        default: self = .failed(detail ?? "Unknown failure")
        }
    }
}

/// Remote display geometry.
public struct DisplayGeometry: Equatable, Sendable {
    public let width: UInt32
    public let height: UInt32
    public let scaleNumerator: UInt32
    public let scaleDenominator: UInt32

    public var scale: Double {
        guard scaleDenominator > 0 else { return 1.0 }
        return Double(scaleNumerator) / Double(scaleDenominator)
    }

    public static let zero = DisplayGeometry(width: 0, height: 0, scaleNumerator: 1, scaleDenominator: 1)
}

/// Connection quality snapshot for UX status indicators.
public struct ConnectionQuality: Equatable, Sendable {
    public let rttMs: UInt32
    public let jitterMs: UInt32
    public let lossPermille: UInt32
    public let fps: UInt32
    public let bitrateKbps: UInt32
    public let decodeTimeUs: UInt32
    public let renderTimeUs: UInt32
    public let qualityTier: QualityTier

    public enum QualityTier: UInt32, Sendable {
        case excellent = 0
        case good = 1
        case degraded = 2
        case poor = 3
    }

    init(cQuality: fr_connection_quality) {
        self.rttMs = cQuality.rtt_ms
        self.jitterMs = cQuality.jitter_ms
        self.lossPermille = cQuality.loss_permille
        self.fps = cQuality.fps
        self.bitrateKbps = cQuality.bitrate_kbps
        self.decodeTimeUs = cQuality.decode_time_us
        self.renderTimeUs = cQuality.render_time_us
        self.qualityTier = QualityTier(rawValue: cQuality.quality_tier) ?? .poor
    }

    public static let initial = ConnectionQuality(
        cQuality: fr_connection_quality(
            rtt_ms: 0, jitter_ms: 0, loss_permille: 0, fps: 0,
            bitrate_kbps: 0, decode_time_us: 0, render_time_us: 0, quality_tier: 0
        )
    )
}

/// Pointer action for remote touch and trackpad input.
public enum PointerAction: UInt32, Sendable {
    case move = 0
    case down = 1
    case up = 2
    case cancel = 3
}

/// Pointer button identifier.
public enum PointerButton: UInt32, Sendable {
    case none = 0
    case primary = 1
    case secondary = 2
    case middle = 3
}

/// Key event action.
public enum KeyAction: UInt32, Sendable {
    case down = 0
    case up = 1
}

/// High-level Swift session controller for FrankenRemote.
@MainActor
public final class FrankenSessionController: ObservableObject {
    @Published public private(set) var state: SessionState = .disconnected
    @Published public private(set) var geometry: DisplayGeometry = .zero
    @Published public private(set) var quality: ConnectionQuality = .initial
    @Published public private(set) var isMicEnabled: Bool = false

    /// Closure invoked when remote clipboard content arrives.
    public var onRemoteClipboardReceived: (@MainActor (String) -> Void)?

    private var sessionHandle: fr_session_handle = 0
    private var isDestroyed: Bool = false
    private var callbackContextPtr: UnsafeMutableRawPointer?

    public init(hostAddress: String, authToken: String) throws {
        _ = fr_mobile_init()

        var handle: fr_session_handle = 0
        let ret = hostAddress.withCString { hostCStr in
            authToken.withCString { authCStr in
                fr_session_create(hostCStr, authCStr, &handle)
            }
        }

        guard ret == FR_OK else {
            throw FrankenError(code: ret)
        }

        self.sessionHandle = handle
        self.setupCallbacks()
    }

    deinit {
        cleanup()
    }

    // MARK: - Lifecycle

    /// Connect to remote host asynchronously.
    public func connect() async throws {
        guard !isDestroyed, sessionHandle != 0 else {
            throw FrankenError.alreadyClosed
        }

        let handle = self.sessionHandle
        let ret = await Task.detached {
            fr_session_connect(handle)
        }.value

        guard ret == FR_OK else {
            throw FrankenError(code: ret)
        }
    }

    /// Disconnect from remote host.
    public func disconnect() {
        guard !isDestroyed, sessionHandle != 0 else { return }
        _ = fr_session_disconnect(sessionHandle)
    }

    /// Explicitly destroy the session and release all Rust core resources.
    public func close() {
        cleanup()
    }

    private func cleanup() {
        guard !isDestroyed else { return }
        isDestroyed = true

        if sessionHandle != 0 {
            // Unregister callbacks first
            var emptyCallbacks = fr_session_callbacks()
            _ = fr_session_set_callbacks(sessionHandle, &emptyCallbacks)
            _ = fr_session_destroy(sessionHandle)
            sessionHandle = 0
        }

        if let ptr = callbackContextPtr {
            Unmanaged<FrankenSessionController>.fromOpaque(ptr).release()
            callbackContextPtr = nil
        }
    }

    // MARK: - Zero-Copy Metal Surface Attachment

    /// Attach a CAMetalLayer for direct zero-copy hardware decode presentation.
    public func attachMetalLayer(_ layer: CAMetalLayer) throws {
        guard !isDestroyed, sessionHandle != 0 else {
            throw FrankenError.alreadyClosed
        }

        let layerPtr = Unmanaged.passUnretained(layer).toOpaque()
        let ret = fr_session_attach_metal_layer(sessionHandle, layerPtr)
        guard ret == FR_OK else {
            throw FrankenError(code: ret)
        }
    }

    // MARK: - Input Submission

    /// Send pointer action (touch / trackpad).
    public func sendPointer(x: Int, y: Int, action: PointerAction, button: PointerButton = .none) {
        guard !isDestroyed, sessionHandle != 0 else { return }
        _ = fr_session_send_pointer(sessionHandle, Int32(x), Int32(y), action.rawValue, button.rawValue)
    }

    /// Send key action (software or hardware keyboard).
    public func sendKey(keyCode: UInt32, action: KeyAction) {
        guard !isDestroyed, sessionHandle != 0 else { return }
        _ = fr_session_send_key(sessionHandle, keyCode, action.rawValue)
    }

    /// Send scroll offset (trackpad 2-finger scroll or wheel).
    public func sendScroll(dx: Int, dy: Int) {
        guard !isDestroyed, sessionHandle != 0 else { return }
        _ = fr_session_send_scroll(sessionHandle, Int32(dx), Int32(dy))
    }

    /// Send typed text directly (IME / virtual keyboard text commit).
    public func sendText(_ text: String) {
        guard !isDestroyed, sessionHandle != 0 else { return }
        _ = text.withCString { cStr in
            fr_session_send_text(sessionHandle, cStr)
        }
    }

    // MARK: - Audio Uplink

    /// Toggle microphone audio uplink to the remote workstation.
    public func setMicEnabled(_ enabled: Bool) {
        guard !isDestroyed, sessionHandle != 0 else { return }
        let ret = fr_session_set_mic_enabled(sessionHandle, enabled)
        if ret == FR_OK {
            self.isMicEnabled = enabled
        }
    }

    // MARK: - Clipboard

    /// Send clipboard string to the remote workstation.
    public func sendClipboard(_ text: String) {
        guard !isDestroyed, sessionHandle != 0 else { return }
        _ = text.withCString { cStr in
            fr_session_send_clipboard(sessionHandle, cStr)
        }
    }

    // MARK: - Internal Callbacks Setup

    private func setupCallbacks() {
        let unmanaged = Unmanaged.passRetained(self)
        let opaquePtr = unmanaged.toOpaque()
        self.callbackContextPtr = opaquePtr

        var callbacks = fr_session_callbacks(
            on_state: { _, state, detailCStr, userData in
                guard let userData = userData else { return }
                let controller = Unmanaged<FrankenSessionController>.fromOpaque(userData).takeUnretainedValue()
                let detail = detailCStr != nil ? String(cString: detailCStr!) : nil
                Task { @MainActor in
                    controller.state = SessionState(cState: state, detail: detail)
                }
            },
            on_geometry: { _, width, height, scaleNum, scaleDen, userData in
                guard let userData = userData else { return }
                let controller = Unmanaged<FrankenSessionController>.fromOpaque(userData).takeUnretainedValue()
                Task { @MainActor in
                    controller.geometry = DisplayGeometry(
                        width: width,
                        height: height,
                        scaleNumerator: scaleNum,
                        scaleDenominator: scaleDen
                    )
                }
            },
            on_quality: { _, qualityPtr, userData in
                guard let userData = userData, let qualityPtr = qualityPtr else { return }
                let controller = Unmanaged<FrankenSessionController>.fromOpaque(userData).takeUnretainedValue()
                let snapshot = ConnectionQuality(cQuality: qualityPtr.pointee)
                Task { @MainActor in
                    controller.quality = snapshot
                }
            },
            on_clipboard: { _, textCStr, userData in
                guard let userData = userData, let textCStr = textCStr else { return }
                let controller = Unmanaged<FrankenSessionController>.fromOpaque(userData).takeUnretainedValue()
                let text = String(cString: textCStr)
                Task { @MainActor in
                    controller.onRemoteClipboardReceived?(text)
                }
            },
            user_data: opaquePtr
        )

        _ = fr_session_set_callbacks(sessionHandle, &callbacks)
    }
}
