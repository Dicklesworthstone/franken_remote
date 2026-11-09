import CoreGraphics
import Foundation

/// Two-finger scroll physics: finger travel becomes fractional lines, and a
/// flick keeps scrolling with exponential deceleration (like UIScrollView's
/// normal rate) until it is slow or a new touch stops it.
struct MomentumScroller: Equatable {
    /// View points of finger travel per line.
    var pointsPerLine: CGFloat = 18
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
        a.pointsPerLine == b.pointsPerLine && a.natural == b.natural && a.velocity == b.velocity
            && a.coasting == b.coasting
    }

    /// Lines to scroll for a finger delta (view points) while touching.
    mutating func drag(_ delta: CGVector, at time: Double) -> (dx: Double, dy: Double) {
        coasting = false
        samples.append((time, delta))
        samples.removeAll { time - $0.time > 0.08 }
        return lines(delta)
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

    /// Advance momentum by `interval` seconds; lines to scroll, or nil when
    /// not coasting (or it just stopped).
    mutating func coast(interval: Double) -> (dx: Double, dy: Double)? {
        guard coasting, interval > 0 else { return nil }
        let decay = CGFloat(exp(-min(interval, 0.1) / timeConstant))
        // Distance travelled over the interval by an exponentially decaying
        // velocity: v * tau * (1 - decay).
        let factor = CGFloat(timeConstant) * (1 - decay)
        let delta = CGVector(dx: velocity.dx * factor, dy: velocity.dy * factor)
        velocity = CGVector(dx: velocity.dx * decay, dy: velocity.dy * decay)
        if (velocity.dx * velocity.dx + velocity.dy * velocity.dy).squareRoot() < stopSpeed {
            coasting = false
        }
        return lines(delta)
    }

    mutating func stop() {
        coasting = false
        velocity = .zero
        samples.removeAll()
    }

    private func lines(_ delta: CGVector) -> (dx: Double, dy: Double) {
        let sign: CGFloat = natural ? -1 : 1
        return (Double(sign * delta.dx / pointsPerLine), Double(sign * delta.dy / pointsPerLine))
    }
}
