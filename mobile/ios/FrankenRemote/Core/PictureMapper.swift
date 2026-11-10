import CoreGraphics

/// Edge insets without UIKit, so the geometry is testable on its own.
struct Insets: Equatable {
    var top: CGFloat = 0
    var left: CGFloat = 0
    var bottom: CGFloat = 0
    var right: CGFloat = 0
}

/// Where the host's picture sits on the phone, and the exact mapping between
/// view points and encoded picture pixels.
///
/// The picture is fitted (letterboxed) into the safe area, then optionally
/// zoomed. `center` is the picture pixel shown at the middle of the content
/// area; on an axis where the zoomed picture is smaller than the content it is
/// centred instead. Rotation and safe-area changes keep zoom and `center`,
/// except that an untouched view stays at its resting zoom.
///
/// With the device's pixel density known (`deviceScale`), the picture's
/// origin sits on the device pixel grid, the resting zoom prefers a whole
/// number of device pixels per picture pixel when that costs at most 15% of
/// the fitted size (sharp instead of an off-integer upscale), and the zoom
/// may go below fit as far as one device pixel per picture pixel.
struct PictureMapper: Equatable {
    var viewSize: CGSize = .zero
    var insets = Insets()
    /// Encoded picture size in pixels; zero until the first picture.
    var picture: CGSize = .zero
    /// Device pixels per view point (the drawable's scale); 0 = unknown: no
    /// grid snapping, and the zoom stays between fit and the maximum.
    var deviceScale: CGFloat = 0
    /// 1 = fit; between `minZoom` and `maxZoom`.
    private(set) var zoom: CGFloat = 1
    /// Picture pixel at the middle of the content area.
    private(set) var center: CGPoint = .zero
    /// The user has not zoomed or panned since the last reset: geometry
    /// changes keep the view at its resting zoom.
    private(set) var resting = true

    /// Most view points one picture pixel may cover when zoomed in.
    static let maxPointsPerPixel: CGFloat = 6

    init(viewSize: CGSize = .zero, insets: Insets = Insets(), picture: CGSize = .zero) {
        self.viewSize = viewSize
        self.insets = insets
        self.picture = picture
        self.center = CGPoint(x: picture.width / 2, y: picture.height / 2)
    }

    var isReady: Bool {
        picture.width > 0 && picture.height > 0 && content.width > 0 && content.height > 0
    }

    /// The safe area, in view points.
    var content: CGRect {
        CGRect(
            x: insets.left,
            y: insets.top,
            width: max(0, viewSize.width - insets.left - insets.right),
            height: max(0, viewSize.height - insets.top - insets.bottom))
    }

    /// View points per picture pixel at zoom 1.
    var fitScale: CGFloat {
        guard isReady else { return 1 }
        return min(content.width / picture.width, content.height / picture.height)
    }

    /// View points per picture pixel now.
    var scale: CGFloat { fitScale * zoom }

    var maxZoom: CGFloat { max(1, Self.maxPointsPerPixel / fitScale) }

    /// Device pixels per picture pixel at fit (0 when unknown).
    private var fitDevicePixels: CGFloat {
        guard isReady, deviceScale > 0 else { return 0 }
        return fitScale * deviceScale
    }

    /// Fit, or one device pixel per picture pixel when fit upscales.
    var minZoom: CGFloat {
        let d = fitDevicePixels
        return d > 1 ? 1 / d : 1
    }

    /// The zoom an untouched view rests at: fit, or the largest whole number
    /// of device pixels per picture pixel when that is at most 15% smaller.
    var restingZoom: CGFloat {
        let d = fitDevicePixels
        let whole = d.rounded(.down)
        guard whole >= 1, d - whole > 0.0001, whole / d >= 0.85 else { return 1 }
        return whole / d
    }

    /// The zoom at which one picture pixel is exactly one device pixel.
    func actualPixelsZoom(screenScale: CGFloat) -> CGFloat {
        guard isReady, screenScale > 0 else { return 1 }
        return min(maxZoom, max(minZoom, 1 / (screenScale * fitScale)))
    }

    /// Device pixels per picture pixel; an integer means pixel-exact drawing.
    func devicePixelsPerPixel(screenScale: CGFloat) -> CGFloat { scale * screenScale }

    /// Where the picture is drawn, in view points; its origin on the device
    /// pixel grid when `deviceScale` is known.
    var pictureRect: CGRect {
        guard isReady else { return .zero }
        let s = scale
        let size = CGSize(width: picture.width * s, height: picture.height * s)
        let grid = deviceScale
        func origin(_ length: CGFloat, _ contentMin: CGFloat, _ contentLength: CGFloat,
                    _ center: CGFloat) -> CGFloat {
            let exact = length <= contentLength
                ? contentMin + (contentLength - length) / 2
                : contentMin + contentLength / 2 - center * s
            return grid > 0 ? (exact * grid).rounded() / grid : exact
        }
        return CGRect(
            x: origin(size.width, content.minX, content.width, center.x),
            y: origin(size.height, content.minY, content.height, center.y),
            width: size.width,
            height: size.height)
    }

    /// The picture pixel under a view point, or nil outside the picture
    /// (half-open: the right and bottom edges are outside). Never clamped.
    func toPicture(_ point: CGPoint) -> CGPoint? {
        guard isReady else { return nil }
        let p = toPictureUnbounded(point)
        guard p.x >= 0, p.y >= 0, p.x < picture.width, p.y < picture.height else { return nil }
        return p
    }

    /// The picture coordinate under a view point, inside the picture or not.
    func toPictureUnbounded(_ point: CGPoint) -> CGPoint {
        let rect = pictureRect
        let s = scale
        return CGPoint(x: (point.x - rect.minX) / s, y: (point.y - rect.minY) / s)
    }

    func toView(_ point: CGPoint) -> CGPoint {
        let rect = pictureRect
        let s = scale
        return CGPoint(x: rect.minX + point.x * s, y: rect.minY + point.y * s)
    }

    /// Zoom by `factor`, keeping the picture point under `anchor` (view
    /// points) under it.
    mutating func zoom(by factor: CGFloat, at anchor: CGPoint) {
        guard isReady, factor.isFinite, factor > 0 else { return }
        setZoom(zoom * factor, at: anchor)
    }

    /// Set the zoom, keeping the picture point under `anchor` fixed.
    mutating func setZoom(_ value: CGFloat, at anchor: CGPoint) {
        guard isReady, value.isFinite else { return }
        let fixed = toPictureUnbounded(anchor)
        zoom = min(maxZoom, max(minZoom, value))
        let s = scale
        center = CGPoint(
            x: fixed.x - (anchor.x - content.midX) / s,
            y: fixed.y - (anchor.y - content.midY) / s)
        clamp()
        resting = abs(zoom - restingZoom) < 1e-9 && fitsBoth
    }

    /// Move the picture with a finger: by `delta` view points.
    mutating func pan(by delta: CGVector) {
        guard isReady, delta.dx.isFinite, delta.dy.isFinite else { return }
        let s = scale
        let before = center
        center.x -= delta.dx / s
        center.y -= delta.dy / s
        clamp()
        if center != before { resting = false }
    }

    /// Back to the resting zoom, centred.
    mutating func reset() {
        zoom = restingZoom
        center = CGPoint(x: picture.width / 2, y: picture.height / 2)
        resting = true
    }

    /// A new picture size: a different size starts again at rest.
    mutating func setPicture(_ size: CGSize) {
        guard size != picture else { return }
        picture = size
        reset()
    }

    /// The view, its safe area or the device scale changed (layout,
    /// rotation): an untouched view settles at its new resting zoom, a
    /// zoomed one keeps its zoom and centre.
    mutating func setGeometry(viewSize: CGSize, insets: Insets, deviceScale: CGFloat) {
        self.viewSize = viewSize
        self.insets = insets
        self.deviceScale = deviceScale.isFinite && deviceScale > 0 ? deviceScale : 0
        if resting {
            reset()
        } else {
            zoom = min(maxZoom, max(minZoom, zoom))
            clamp()
        }
    }

    /// The whole picture is inside the content area.
    private var fitsBoth: Bool {
        let s = scale
        return picture.width * s <= content.width + 1e-9 && picture.height * s <= content.height + 1e-9
    }

    /// Keep the content area covered on every axis the picture overfills.
    mutating func clamp() {
        guard isReady else { return }
        let s = scale
        let halfW = content.width / (2 * s)
        let halfH = content.height / (2 * s)
        center.x = picture.width * s <= content.width
            ? picture.width / 2 : min(max(center.x, halfW), picture.width - halfW)
        center.y = picture.height * s <= content.height
            ? picture.height / 2 : min(max(center.y, halfH), picture.height - halfH)
    }

    /// The visible part of the picture, in picture pixels.
    var visiblePicture: CGRect {
        guard isReady else { return .zero }
        let rect = pictureRect.intersection(content)
        let a = toPictureUnbounded(CGPoint(x: rect.minX, y: rect.minY))
        let b = toPictureUnbounded(CGPoint(x: rect.maxX, y: rect.maxY))
        return CGRect(x: a.x, y: a.y, width: b.x - a.x, height: b.y - a.y)
    }

    /// Pan just enough that `point` (picture pixels) sits at least `margin`
    /// view points inside the content area. Returns whether it panned.
    @discardableResult
    mutating func reveal(_ point: CGPoint, margin: CGFloat) -> Bool {
        guard isReady else { return false }
        let v = toView(point)
        let inner = content.insetBy(dx: min(margin, content.width / 3), dy: min(margin, content.height / 3))
        var dx: CGFloat = 0
        var dy: CGFloat = 0
        if v.x < inner.minX { dx = inner.minX - v.x } else if v.x > inner.maxX { dx = inner.maxX - v.x }
        if v.y < inner.minY { dy = inner.minY - v.y } else if v.y > inner.maxY { dy = inner.maxY - v.y }
        guard dx != 0 || dy != 0 else { return false }
        let before = center
        pan(by: CGVector(dx: dx, dy: dy))
        return center != before
    }
}
