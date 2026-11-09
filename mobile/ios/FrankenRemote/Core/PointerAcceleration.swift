import CoreGraphics

/// Trackpad-mode pointer motion: finger travel in view points becomes host
/// pointer travel in picture pixels. At `speed` 1 and a slow finger the
/// pointer follows the finger exactly as drawn on screen (so zooming in gives
/// finer control); faster swipes travel further, smoothly, up to `maxGain`.
struct PointerAcceleration: Equatable {
    /// User setting, 0.5...2.
    var speed: CGFloat = 1
    /// Below this finger speed (points/s) there is no acceleration.
    var threshold: CGFloat = 120
    /// Finger speed (points/s) at which the gain reaches `maxGain`.
    var saturation: CGFloat = 1400
    var maxGain: CGFloat = 2.6

    /// The gain for a finger moving at `velocity` points per second.
    func gain(velocity: CGFloat) -> CGFloat {
        guard velocity.isFinite, velocity > threshold else { return 1 }
        let t = min(1, (velocity - threshold) / (saturation - threshold))
        // Smoothstep: no kink where acceleration starts or saturates.
        let eased = t * t * (3 - 2 * t)
        return 1 + (maxGain - 1) * eased
    }

    /// Picture-pixel travel for a finger delta over `interval` seconds, with
    /// the picture drawn at `scale` view points per picture pixel.
    func travel(_ delta: CGVector, interval: Double, scale: CGFloat) -> CGVector {
        guard scale > 0, interval >= 0 else { return .zero }
        let distance = (delta.dx * delta.dx + delta.dy * delta.dy).squareRoot()
        let velocity = interval > 0 ? distance / CGFloat(interval) : 0
        let factor = speed * gain(velocity: velocity) / scale
        return CGVector(dx: delta.dx * factor, dy: delta.dy * factor)
    }
}
