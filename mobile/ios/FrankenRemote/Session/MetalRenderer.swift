import CoreVideo
import Metal
import MetalKit
import QuartzCore

/// Draws the newest decoded picture (IOSurface NV12, zero-copy through a
/// CVMetalTextureCache) where the `PictureMapper` puts it, and reports each
/// picture that reached the screen back to the session: the host's control
/// grant depends on that evidence.
final class MetalRenderer: NSObject, MTKViewDelegate {
    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private let picturePipeline: MTLRenderPipelineState
    private let cursorPipeline: MTLRenderPipelineState
    private var cache: CVMetalTextureCache?

    weak var session: RemoteSession?
    /// Called first on every display refresh (event pump, gesture timers).
    var onRefresh: ((CFTimeInterval) -> Void)?
    /// The current geometry.
    var mapper: () -> PictureMapper = { PictureMapper() }

    private struct Picture {
        let luma: MTLTexture
        let chroma: MTLTexture
        // The CoreVideo wrappers keep the IOSurface planes alive while the
        // GPU reads them.
        let wrappers: [CVMetalTexture]
        let buffer: CVPixelBuffer
        let info: FrFrameInfo
    }
    private var current: Picture?
    private var lastSequence: UInt64 = 0
    private var lastMapper = PictureMapper()
    private var lastDrawableSize = CGSize.zero
    private var cursorTexture: MTLTexture?
    private var cursor: FrCursor?
    private var dirty = true

    /// Content-free counters for the stats overlay.
    private(set) var drawnPictures: UInt64 = 0
    private(set) var presentedPictures: UInt64 = 0

    init?(view: MTKView) {
        guard let device = view.device ?? MTLCreateSystemDefaultDevice(),
              let queue = device.makeCommandQueue(),
              let library = device.makeDefaultLibrary(),
              let vertex = library.makeFunction(name: "picture_vertex"),
              let pictureFragment = library.makeFunction(name: "picture_fragment"),
              let cursorFragment = library.makeFunction(name: "cursor_fragment") else { return nil }
        self.device = device
        self.queue = queue
        let picture = MTLRenderPipelineDescriptor()
        picture.vertexFunction = vertex
        picture.fragmentFunction = pictureFragment
        picture.colorAttachments[0].pixelFormat = view.colorPixelFormat
        let cursor = MTLRenderPipelineDescriptor()
        cursor.vertexFunction = vertex
        cursor.fragmentFunction = cursorFragment
        cursor.colorAttachments[0].pixelFormat = view.colorPixelFormat
        cursor.colorAttachments[0].isBlendingEnabled = true
        cursor.colorAttachments[0].sourceRGBBlendFactor = .sourceAlpha
        cursor.colorAttachments[0].destinationRGBBlendFactor = .oneMinusSourceAlpha
        cursor.colorAttachments[0].sourceAlphaBlendFactor = .one
        cursor.colorAttachments[0].destinationAlphaBlendFactor = .oneMinusSourceAlpha
        do {
            picturePipeline = try device.makeRenderPipelineState(descriptor: picture)
            cursorPipeline = try device.makeRenderPipelineState(descriptor: cursor)
        } catch {
            return nil
        }
        var cache: CVMetalTextureCache?
        CVMetalTextureCacheCreate(kCFAllocatorDefault, nil, device, nil, &cache)
        self.cache = cache
        super.init()
        view.device = device
    }

    /// Drop the shown picture (a new session or a new size).
    func reset() {
        current = nil
        lastSequence = 0
        cursor = nil
        cursorTexture = nil
        dirty = true
        if let cache { CVMetalTextureCacheFlush(cache, 0) }
    }

    func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {
        dirty = true
    }

    func draw(in view: MTKView) {
        let now = CACurrentMediaTime()
        onRefresh?(now)
        guard let session else { return }
        if let frame = session.frame(after: lastSequence), let picture = makePicture(frame) {
            current = picture
            lastSequence = frame.info.sequence
            dirty = true
        }
        if let shape = session.cursor(knownGeneration: cursor?.generation ?? 0) {
            updateCursor(shape)
            dirty = true
        }
        let geometry = mapper()
        if geometry != lastMapper || view.drawableSize != lastDrawableSize {
            lastMapper = geometry
            lastDrawableSize = view.drawableSize
            dirty = true
        }
        guard dirty, let current, geometry.isReady,
              let pass = view.currentRenderPassDescriptor,
              let drawable = view.currentDrawable,
              let commands = queue.makeCommandBuffer(),
              let encoder = commands.makeRenderCommandEncoder(descriptor: pass) else { return }
        dirty = false
        let bounds = view.bounds.size
        let drawableSize = view.drawableSize
        let sx = drawableSize.width / max(1, bounds.width)
        let sy = drawableSize.height / max(1, bounds.height)
        let content = geometry.content
        if let scissor = scissorRect(content, sx: sx, sy: sy, drawable: drawableSize) {
            encoder.setScissorRect(scissor)
        }
        let rect = geometry.pictureRect
        let devicePerPixel = geometry.scale * sx
        let exact = abs(devicePerPixel - devicePerPixel.rounded()) < 0.001 && devicePerPixel >= 0.999
        var uniforms = PictureUniforms(
            rect: clipRect(rect, sx: sx, sy: sy, drawable: drawableSize),
            textureSize: SIMD2(Float(current.luma.width), Float(current.luma.height)),
            fullRange: current.info.full_range != 0 ? 1 : 0,
            exact: exact ? 1 : 0)
        encoder.setRenderPipelineState(picturePipeline)
        encoder.setVertexBytes(&uniforms, length: MemoryLayout<PictureUniforms>.stride, index: 0)
        encoder.setFragmentBytes(&uniforms, length: MemoryLayout<PictureUniforms>.stride, index: 0)
        encoder.setFragmentTexture(current.luma, index: 0)
        encoder.setFragmentTexture(current.chroma, index: 1)
        encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        if let cursor, cursor.visible != 0, let texture = cursorTexture {
            let origin = geometry.toView(CGPoint(x: Int(cursor.x) - Int(cursor.hotspot_x),
                                                 y: Int(cursor.y) - Int(cursor.hotspot_y)))
            let size = CGSize(width: CGFloat(cursor.width) * geometry.scale,
                              height: CGFloat(cursor.height) * geometry.scale)
            var shape = PictureUniforms(
                rect: clipRect(CGRect(origin: origin, size: size), sx: sx, sy: sy, drawable: drawableSize),
                textureSize: SIMD2(Float(texture.width), Float(texture.height)), fullRange: 0, exact: 1)
            encoder.setRenderPipelineState(cursorPipeline)
            encoder.setVertexBytes(&shape, length: MemoryLayout<PictureUniforms>.stride, index: 0)
            encoder.setFragmentTexture(texture, index: 0)
            encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        }
        encoder.endEncoding()
        let frame = current.info.frame
        let handle = session.handle
        drawnPictures += 1
        #if targetEnvironment(simulator)
        // The simulator's Metal has no presentation feedback: the nearest
        // evidence is the GPU finishing a drawable already queued to present.
        commands.addCompletedHandler { [weak self] buffer in
            guard buffer.status == .completed else { return }
            fr_session_frame_drawn(handle, frame)
            DispatchQueue.main.async { self?.presentedPictures += 1 }
        }
        #else
        drawable.addPresentedHandler { [weak self] presented in
            // Only a drawable that actually reached the display counts.
            guard presented.presentedTime > 0 else { return }
            fr_session_frame_drawn(handle, frame)
            DispatchQueue.main.async { self?.presentedPictures += 1 }
        }
        #endif
        commands.present(drawable)
        // Keep this picture's planes alive until the GPU is done with them.
        commands.addCompletedHandler { _ in _ = current }
        commands.commit()
    }

    private func makePicture(_ frame: RemoteSession.Frame) -> Picture? {
        guard let cache else { return nil }
        let buffer = frame.buffer
        guard CVPixelBufferGetPlaneCount(buffer) == 2 else { return nil }
        func plane(_ index: Int, _ format: MTLPixelFormat) -> (MTLTexture, CVMetalTexture)? {
            let width = CVPixelBufferGetWidthOfPlane(buffer, index)
            let height = CVPixelBufferGetHeightOfPlane(buffer, index)
            var wrapper: CVMetalTexture?
            let status = CVMetalTextureCacheCreateTextureFromImage(
                kCFAllocatorDefault, cache, buffer, nil, format, width, height, index, &wrapper)
            guard status == kCVReturnSuccess, let wrapper,
                  let texture = CVMetalTextureGetTexture(wrapper) else { return nil }
            return (texture, wrapper)
        }
        guard let luma = plane(0, .r8Unorm), let chroma = plane(1, .rg8Unorm) else { return nil }
        return Picture(luma: luma.0, chroma: chroma.0, wrappers: [luma.1, chroma.1],
                       buffer: buffer, info: frame.info)
    }

    private func updateCursor(_ shape: RemoteSession.CursorShape) {
        cursor = shape.state
        let width = Int(shape.state.width)
        let height = Int(shape.state.height)
        guard width > 0, height > 0, shape.rgba.count == width * height * 4 else {
            cursorTexture = nil
            return
        }
        let descriptor = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .rgba8Unorm, width: width, height: height, mipmapped: false)
        descriptor.usage = .shaderRead
        guard let texture = device.makeTexture(descriptor: descriptor) else { return }
        shape.rgba.withUnsafeBytes { bytes in
            if let base = bytes.baseAddress {
                texture.replace(region: MTLRegionMake2D(0, 0, width, height), mipmapLevel: 0,
                                withBytes: base, bytesPerRow: width * 4)
            }
        }
        cursorTexture = texture
    }

    private func clipRect(_ rect: CGRect, sx: CGFloat, sy: CGFloat, drawable: CGSize) -> SIMD4<Float> {
        let left = rect.minX * sx / drawable.width * 2 - 1
        let right = rect.maxX * sx / drawable.width * 2 - 1
        let top = 1 - rect.minY * sy / drawable.height * 2
        let bottom = 1 - rect.maxY * sy / drawable.height * 2
        return SIMD4(Float(left), Float(top), Float(right), Float(bottom))
    }

    private func scissorRect(_ rect: CGRect, sx: CGFloat, sy: CGFloat, drawable: CGSize) -> MTLScissorRect? {
        let x = max(0, Int((rect.minX * sx).rounded()))
        let y = max(0, Int((rect.minY * sy).rounded()))
        let right = min(Int(drawable.width), Int((rect.maxX * sx).rounded()))
        let bottom = min(Int(drawable.height), Int((rect.maxY * sy).rounded()))
        guard right > x, bottom > y else { return nil }
        return MTLScissorRect(x: x, y: y, width: right - x, height: bottom - y)
    }
}

/// Matches `PictureUniforms` in Shaders.metal.
struct PictureUniforms {
    var rect: SIMD4<Float>
    var textureSize: SIMD2<Float>
    var fullRange: Float
    var exact: Float
}
