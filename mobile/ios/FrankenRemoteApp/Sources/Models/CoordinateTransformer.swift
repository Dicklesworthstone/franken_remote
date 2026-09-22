//
// CoordinateTransformer.swift — Viewport & Touch Coordinate Transformation
//
// Converts view space coordinates to host desktop space with aspect-fit,
// pan/zoom transformation, bounds clamping, and generation fencing.
//

import Foundation
import CoreGraphics

public struct CoordinateTransformer: Equatable, Sendable {
    public var desktopWidth: UInt32
    public var desktopHeight: UInt32
    public var viewportSize: CGSize
    public var zoomScale: CGFloat
    public var panOffset: CGPoint

    public init(
        desktopWidth: UInt32 = 1920,
        desktopHeight: UInt32 = 1080,
        viewportSize: CGSize = .zero,
        zoomScale: CGFloat = 1.0,
        panOffset: CGPoint = .zero
    ) {
        self.desktopWidth = desktopWidth
        self.desktopHeight = desktopHeight
        self.viewportSize = viewportSize
        self.zoomScale = max(1.0, min(zoomScale, 5.0))
        self.panOffset = panOffset
    }

    /// Calculate aspect-fit display rectangle within the current viewport.
    public var fittedRect: CGRect {
        guard desktopWidth > 0, desktopHeight > 0,
              viewportSize.width > 0, viewportSize.height > 0 else {
            return .zero
        }

        let aspectDesktop = CGFloat(desktopWidth) / CGFloat(desktopHeight)
        let aspectView = viewportSize.width / viewportSize.height

        var width: CGFloat
        var height: CGFloat

        if aspectView > aspectDesktop {
            height = viewportSize.height * zoomScale
            width = height * aspectDesktop
        } else {
            width = viewportSize.width * zoomScale
            height = width / aspectDesktop
        }

        let originX = (viewportSize.width - width) / 2.0 + panOffset.x
        let originY = (viewportSize.height - height) / 2.0 + panOffset.y

        return CGRect(x: originX, y: originY, width: width, height: height)
    }

    /// Map a point from viewport coordinate space to remote desktop coordinate space.
    /// Returns nil if the point falls outside the active video bounds.
    public func viewPointToDesktopPoint(_ point: CGPoint) -> (x: Int32, y: Int32)? {
        let rect = fittedRect
        guard rect.width > 0, rect.height > 0 else { return nil }

        let localX = point.x - rect.origin.x
        let localY = point.y - rect.origin.y

        guard localX >= 0, localX <= rect.width,
              localY >= 0, localY <= rect.height else {
            return nil
        }

        let normX = localX / rect.width
        let normY = localY / rect.height

        let desktopX = Int32(normX * CGFloat(desktopWidth))
        let desktopY = Int32(normY * CGFloat(desktopHeight))

        let clampedX = max(0, min(desktopX, Int32(desktopWidth - 1)))
        let clampedY = max(0, min(desktopY, Int32(desktopHeight - 1)))

        return (clampedX, clampedY)
    }

    /// Apply relative translation in desktop space for trackpad mode.
    public func applyTrackpadDelta(
        currentDesktop: (x: Int32, y: Int32),
        deltaView: CGPoint,
        sensitivity: CGFloat = 1.5
    ) -> (x: Int32, y: Int32) {
        let rect = fittedRect
        guard rect.width > 0, rect.height > 0 else { return currentDesktop }

        let scaleX = CGFloat(desktopWidth) / rect.width
        let scaleY = CGFloat(desktopHeight) / rect.height

        let deltaDesktopX = deltaView.x * scaleX * sensitivity
        let deltaDesktopY = deltaView.y * scaleY * sensitivity

        let newX = Int32(CGFloat(currentDesktop.x) + deltaDesktopX)
        let newY = Int32(CGFloat(currentDesktop.y) + deltaDesktopY)

        let clampedX = max(0, min(newX, Int32(desktopWidth - 1)))
        let clampedY = max(0, min(newY, Int32(desktopHeight - 1)))

        return (clampedX, clampedY)
    }
}
