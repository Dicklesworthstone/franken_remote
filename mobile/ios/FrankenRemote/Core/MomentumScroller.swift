import CoreGraphics
import Foundation

/// Two-finger scroll physics. The gesture chooses a line-step or precise
/// logical-distance producer before the first delta, and keeps it through
/// momentum. A host refusal never converts or retries one mode as the other.
struct MomentumScroller: Equatable {
    /// View points of finger travel per line.
    var pointsPerLine: CGFloat = 18
    /// User-selected speed for the precise producer. View zoom, encoded
    /// picture dimensions and display density never enter this conversion.
    var preciseScale: CGFloat = 1
    private(set) var precise = false
    /// Natural scrolling: content follows the finger (finger up = scroll down).
    var natural = true
    /// Velocity decays by e every `timeConstant` seconds.
    var timeConstant: Double = 0.33
    /// Below this speed (points/s) momentum stops.
    var stopSpeed: CGFloat = 25
    /// A lift faster than this (points/s) starts momentum.
    var flickSpeed: CGFloat = 180

    private(set) var velocity: CGVector = .zero
    private(set) var coasting = false
    private var samples: [(time: Double, delta: CGVector)] = []

    static func == (a: MomentumScroller, b: MomentumScroller) -> Bool {
        a.pointsPerLine == b.pointsPerLine && a.preciseScale == b.preciseScale
            && a.precise == b.precise && a.natural == b.natural
            && a.velocity == b.velocity && a.coasting == b.coasting
    }

    mutating func begin(precise: Bool) {
        stop()
        self.precise = precise
    }

    /// Distances in the gesture's original unit for one view-point delta.
    mutating func drag(_ delta: CGVector, at time: Double) -> (dx: Double, dy: Double) {
        coasting = false
        samples.append((time, delta))
        samples.removeAll { time - $0.time > 0.08 }
        if samples.count > 32 { samples.removeFirst(samples.count - 32) }
        return distances(delta)
    }

    /// Fingers lifted at `time`: start coasting on a flick.
    mutating func lift(at time: Double) {
        let recent = samples.filter { time - $0.time <= 0.08 }
        samples.removeAll()
        guard let first = recent.first, recent.count >= 2 else {
            coasting = false
            return
        }
        let span = max(1.0 / 120, time - first.time)
        let sum = recent.reduce(CGVector.zero) { CGVector(dx: $0.dx + $1.delta.dx, dy: $0.dy + $1.delta.dy) }
        let v = CGVector(dx: sum.dx / CGFloat(span), dy: sum.dy / CGFloat(span))
        let speed = (v.dx * v.dx + v.dy * v.dy).squareRoot()
        velocity = v
        coasting = speed > flickSpeed
    }

    /// Advance momentum in its original unit. A delayed display callback
    /// discards old momentum instead of playing it into a later fresh view.
    mutating func coast(interval: Double) -> (dx: Double, dy: Double)? {
        guard coasting, interval > 0 else { return nil }
        guard interval.isFinite, interval <= 0.1 else { stop(); return nil }
        let decay = CGFloat(exp(-interval / timeConstant))
        // Distance travelled over the interval by an exponentially decaying
        // velocity: v * tau * (1 - decay).
        let factor = CGFloat(timeConstant) * (1 - decay)
        let delta = CGVector(dx: velocity.dx * factor, dy: velocity.dy * factor)
        velocity = CGVector(dx: velocity.dx * decay, dy: velocity.dy * decay)
        if (velocity.dx * velocity.dx + velocity.dy * velocity.dy).squareRoot() < stopSpeed {
            coasting = false
        }
        return distances(delta)
    }

    mutating func stop() {
        coasting = false
        velocity = .zero
        samples.removeAll()
        precise = false
    }

    private func distances(_ delta: CGVector) -> (dx: Double, dy: Double) {
        let sign: CGFloat = natural ? -1 : 1
        let scale = precise ? preciseScale : 1 / pointsPerLine
        return (Double(sign * delta.dx * scale), Double(sign * delta.dy * scale))
    }
}
