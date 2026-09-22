//
// MetalVideoView.swift — SwiftUI zero-copy CAMetalLayer host for FrankenRemote
//
// Hosts a hardware-accelerated CAMetalLayer wrapped as a SwiftUI View,
// passing the layer pointer directly to the Rust core for zero-copy presentation.
//

import SwiftUI
import QuartzCore

#if canImport(UIKit)
import UIKit

public struct MetalVideoView: UIViewRepresentable {
    @ObservedObject public var controller: FrankenSessionController

    public init(controller: FrankenSessionController) {
        self.controller = controller
    }

    public func makeUIView(context: Context) -> MetalHostingUIView {
        let view = MetalHostingUIView()
        if let metalLayer = view.layer as? CAMetalLayer {
            try? controller.attachMetalLayer(metalLayer)
        }
        return view
    }

    public func updateUIView(_ uiView: MetalHostingUIView, context: Context) {
        // Layout updates handled automatically by MetalHostingUIView
    }
}

public final class MetalHostingUIView: UIView {
    public override class var layerClass: AnyClass {
        return CAMetalLayer.self
    }

    public var metalLayer: CAMetalLayer {
        return layer as! CAMetalLayer
    }

    public override init(frame: CGRect) {
        super.init(frame: frame)
        configureLayer()
    }

    public required init?(coder: NSCoder) {
        super.init(coder: coder)
        configureLayer()
    }

    private func configureLayer() {
        metalLayer.pixelFormat = .bgra8Unorm
        metalLayer.framebufferOnly = true
        metalLayer.contentsScale = UIScreen.main.scale
        backgroundColor = .black
    }
}

#elseif canImport(AppKit)
import AppKit

public struct MetalVideoView: NSViewRepresentable {
    @ObservedObject public var controller: FrankenSessionController

    public init(controller: FrankenSessionController) {
        self.controller = controller
    }

    public func makeNSView(context: Context) -> MetalHostingNSView {
        let view = MetalHostingNSView()
        if let metalLayer = view.metalLayer {
            try? controller.attachMetalLayer(metalLayer)
        }
        return view
    }

    public func updateNSView(_ nsView: MetalHostingNSView, context: Context) {
        // Layout updates handled automatically by MetalHostingNSView
    }
}

public final class MetalHostingNSView: NSView {
    public var metalLayer: CAMetalLayer? {
        return layer as? CAMetalLayer
    }

    public override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        configureLayer()
    }

    public required init?(coder: NSCoder) {
        super.init(coder: coder)
        configureLayer()
    }

    private func configureLayer() {
        wantsLayer = true
        let mLayer = CAMetalLayer()
        mLayer.pixelFormat = .bgra8Unorm
        mLayer.framebufferOnly = true
        layer = mLayer
    }
}

#endif
