import Foundation

/// One row of the original approved catalog. The host's opaque alias never
/// enters Swift; the row is meaningful only with DisplaySelection.token.
struct DisplayOption: Identifiable, Equatable {
    let id: UInt32
    let pixelWidth: UInt32
    let pixelHeight: UInt32
    let logicalWidth: UInt32
    let logicalHeight: UInt32
    let x: Int32
    let y: Int32
    let scaleNumerator: UInt32
    let scaleDenominator: UInt32
    let rotation: UInt32

    init(index: UInt32, info: FrDisplayInfo) {
        id = index
        pixelWidth = info.pixel_width
        pixelHeight = info.pixel_height
        logicalWidth = info.logical_width
        logicalHeight = info.logical_height
        x = info.x
        y = info.y
        scaleNumerator = info.scale_numerator
        scaleDenominator = info.scale_denominator
        rotation = info.rotation
    }

    var title: String { "Display \(id + 1)" }
    var pixels: String { "\(pixelWidth) × \(pixelHeight) pixels" }
    var placement: String { "Desktop position \(x), \(y)" }
    var details: String {
        let scale = Double(scaleNumerator) / Double(scaleDenominator)
        let scaling = String(format: "%.2g× scale", scale)
        return rotation == 0 ? scaling : "\(scaling) · \(rotation * 90)° rotation"
    }
}

/// A complete snapshot of one pending request. Copying never selects a row
/// and incomplete snapshots never become a list of choices.
struct DisplaySelection: Identifiable, Equatable {
    let token: UInt64
    let displays: [DisplayOption]
    var id: UInt64 { token }

    static func copy(_ request: FrDisplayRequest,
                     row: (UInt64, UInt32) -> FrDisplayInfo?) -> DisplaySelection? {
        guard request.token != 0, request.count > 0,
              request.count <= UInt32(FR_MAX_DISPLAYS), request.reserved == 0 else { return nil }
        var displays: [DisplayOption] = []
        for index in 0..<request.count {
            guard let info = row(request.token, index), info.reserved == 0,
                  info.pixel_width > 0, info.pixel_height > 0,
                  info.logical_width > 0, info.logical_height > 0,
                  info.scale_numerator > 0, info.scale_denominator > 0,
                  info.rotation <= 3 else { return nil }
            displays.append(DisplayOption(index: index, info: info))
        }
        return DisplaySelection(token: request.token, displays: displays)
    }
}

/// The UI waits for Rust's actual cleanup result, then the serial host's
/// 400 ms listener cooldown. Neither an end event nor elapsed time is proof
/// that the previous service stopped. A refusal/timeout spends this switch.
struct DisplayReopen {
    enum Step: Equatable { case waiting, ready, refused }
    private var readyAt: Duration?
    private var ended = false

    mutating func poll(status: Int32, elapsed: Duration) -> Step {
        guard !ended, elapsed >= .zero, elapsed < .seconds(10) else {
            ended = true
            return .refused
        }
        switch status {
        case FR_PENDING where readyAt == nil:
            return .waiting
        case FR_OK:
            if readyAt == nil { readyAt = elapsed + .milliseconds(400) }
            guard let readyAt, elapsed >= readyAt else { return .waiting }
            ended = true
            return .ready
        default:
            ended = true
            return .refused
        }
    }

    /// Keep the original destination, port and dial mode. In particular, a
    /// concurrently edited saved row cannot redirect a display switch.
    static func pinnedHost(_ original: SavedHost, observedPin: String?) -> SavedHost? {
        var host = original
        if let observedPin, !observedPin.isEmpty { host.pin = observedPin }
        guard let pin = host.pin, !pin.isEmpty else { return nil }
        return host
    }
}
