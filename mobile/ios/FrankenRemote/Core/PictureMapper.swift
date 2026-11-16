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
    /// Original selected display pixels. Encoded rounding never changes the
    /// input or cursor coordinate space.
    private(set) var display: CGSize = .zero
    /// The retained frame whose owner supplied this geometry; 0 in pure
    /// geometry fixtures. Native callers revalidate it before use.
    private(set) var frame: UInt64 = 0
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

    init(viewSize: CGSize = .zero, insets: Insets = Insets(), picture: CGSize = .zero,
         display: CGSize? = nil) {
        self.viewSize = viewSize
        self.insets = insets
        self.picture = picture
        self.display = display ?? picture
        self.center = CGPoint(x: imageBounds.midX, y: imageBounds.midY)
    }

    var isReady: Bool {
        !imageBounds.isEmpty && content.width > 0 && content.height > 0
    }

    /// Rust's viewport fits the original display into this encoded-size
    /// input area using integer division, including its half-open bars.
    /// Products of two UInt32 dimensions fit UInt64.
    var imageBounds: CGRect {
        guard let sw = UInt32(exactly: display.width), sw > 0,
              let sh = UInt32(exactly: display.height), sh > 0,
              let aw = UInt32(exactly: picture.width), aw > 0,
              let ah = UInt32(exactly: picture.height), ah > 0 else { return .zero }
        let width: UInt64
        let height: UInt64
        if UInt64(aw) * UInt64(sh) <= UInt64(ah) * UInt64(sw) {
            width = UInt64(aw)
            height = UInt64(aw) * UInt64(sh) / UInt64(sw)
        } else {
            width = UInt64(ah) * UInt64(sw) / UInt64(sh)
            height = UInt64(ah)
        }
        guard width > 0, height > 0 else { return .zero }
        return CGRect(x: CGFloat((UInt64(aw) - width) / 2),
                      y: CGFloat((UInt64(ah) - height) / 2),
                      width: CGFloat(width), height: CGFloat(height))
    }

    /// The actual image in view points. The full decoded texture is sampled
    /// into this rectangle; encoded-space rounding bars stay black.
    var imageRect: CGRect {
        guard isReady else { return .zero }
        let image = imageBounds
        return CGRect(origin: toView(image.origin),
                      size: CGSize(width: image.width * scale, height: image.height * scale))
    }

    func containsPicture(_ point: CGPoint) -> Bool {
        let image = imageBounds
        return isReady && point.x.isFinite && point.y.isFinite
            && point.x >= image.minX && point.y >= image.minY
            && point.x < image.maxX && point.y < image.maxY
    }

    /// A cursor hotspot in original display pixels, expressed in the same
    /// encoded-size input area used by Rust. Outside cursors may be clipped.
    func picturePoint(forDisplay point: CGPoint) -> CGPoint? {
        guard isReady, point.x.isFinite, point.y.isFinite else { return nil }
        let image = imageBounds
        return CGPoint(x: image.minX + point.x * image.width / display.width,
                       y: image.minY + point.y * image.height / display.height)
    }

    /// Cursor shape and hotspot dimensions are original display pixels,
    /// while its chosen position is in the encoded-size input area.
    func cursorRect(at point: CGPoint, size: CGSize, hotspot: CGPoint) -> CGRect? {
        guard isReady, point.x.isFinite, point.y.isFinite,
              size.width.isFinite, size.height.isFinite,
              size.width > 0, size.height > 0,
              hotspot.x.isFinite, hotspot.y.isFinite else { return nil }
        let image = imageBounds
        let sx = image.width / display.width
        let sy = image.height / display.height
        let origin = toView(CGPoint(x: point.x - hotspot.x * sx,
                                   y: point.y - hotspot.y * sy))
        return CGRect(origin: origin,
                      size: CGSize(width: size.width * sx * scale, height: size.height * sy * scale))
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
        let image = imageBounds
        return min(content.width / image.width, content.height / image.height)
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

    /// The encoded-size coordinate canvas in view points. Only `imageRect`
    /// is drawn; its origin sits on the device pixel grid when known.
    var pictureRect: CGRect {
        guard isReady else { return .zero }
        let s = scale
        let image = imageBounds
        let grid = deviceScale
        func origin(_ imageMin: CGFloat, _ imageLength: CGFloat,
                    _ contentMin: CGFloat, _ contentLength: CGFloat, _ center: CGFloat) -> CGFloat {
            let length = imageLength * s
            let exact = length <= contentLength
                ? contentMin + (contentLength - length) / 2
                : contentMin + contentLength / 2 - (center - imageMin) * s
            let snapped = grid > 0 ? (exact * grid).rounded() / grid : exact
            return snapped - imageMin * s
        }
        return CGRect(
            x: origin(image.minX, image.width, content.minX, content.width, center.x),
            y: origin(image.minY, image.height, content.minY, content.height, center.y),
            width: picture.width * s,
            height: picture.height * s)
    }

    /// The picture pixel under a view point, or nil outside the picture
    /// (half-open: the right and bottom edges are outside). Never clamped.
    func toPicture(_ point: CGPoint) -> CGPoint? {
        guard isReady else { return nil }
        let p = toPictureUnbounded(point)
        guard containsPicture(p) else { return nil }
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
        center = CGPoint(x: imageBounds.midX, y: imageBounds.midY)
        resting = true
    }

    /// A changed picture or original display starts again at rest. A newer
    /// frame from the same owner and geometry preserves zoom and pan.
    mutating func setPicture(_ size: CGSize, display original: CGSize? = nil, frame: UInt64 = 0) {
        let original = original ?? size
        self.frame = frame
        guard size != picture || original != display else { return }
        picture = size
        display = original
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
        let image = imageBounds
        return image.width * s <= content.width + 1e-9 && image.height * s <= content.height + 1e-9
    }

    /// Keep the content area covered on every axis the picture overfills.
    mutating func clamp() {
        guard isReady else { return }
        let s = scale
        let halfW = content.width / (2 * s)
        let halfH = content.height / (2 * s)
        let image = imageBounds
        center.x = image.width * s <= content.width
            ? image.midX : min(max(center.x, image.minX + halfW), image.maxX - halfW)
        center.y = image.height * s <= content.height
            ? image.midY : min(max(center.y, image.minY + halfH), image.maxY - halfH)
    }

    /// The visible part of the picture, in picture pixels.
    var visiblePicture: CGRect {
        guard isReady else { return .zero }
        let rect = imageRect.intersection(content)
        guard !rect.isNull, !rect.isEmpty else { return .zero }
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
