//
// SessionViewerContainerView.swift — Full Native Session Viewer with Metal Presentation
//
// Hosts CAMetalLayer zero-copy video presentation, interactive gestures for
// Trackpad and Direct-Touch modes, discoverable input toolbar, and quality HUD.
//

import SwiftUI
import FrankenRemoteKit
import AVFoundation
import UniformTypeIdentifiers

public struct SessionViewerContainerView: View {
    @ObservedObject public var controller: FrankenSessionController
    @ObservedObject public var lifecycleManager: AppLifecycleManager

    public let onExit: () -> Void

    @State private var touchMode: TouchMode = .directTouch
    @State private var isKeyboardVisible: Bool = false
    @State private var isModifierBarVisible: Bool = false
    @State private var isQualityExpanded: Bool = false
    @State private var isDocumentPickerPresented: Bool = false

    // Coordinate & Viewport state
    @State private var transformer = CoordinateTransformer()
    @State private var virtualCursorPosition: CGPoint = CGPoint(x: 200, y: 300)
    @State private var isVirtualCursorPressed: Bool = false
    @State private var zoomScale: CGFloat = 1.0
    @State private var panOffset: CGPoint = .zero

    public init(
        controller: FrankenSessionController,
        lifecycleManager: AppLifecycleManager,
        onExit: @escaping () -> Void
    ) {
        self.controller = controller
        self.lifecycleManager = lifecycleManager
        self.onExit = onExit
    }

    public var body: some View {
        GeometryReader { geometry in
            ZStack {
                Color.black.ignoresSafeArea()

                // Hardware-accelerated Metal video presentation
                MetalVideoView(controller: controller)
                    .frame(
                        width: transformer.fittedRect.width,
                        height: transformer.fittedRect.height
                    )
                    .position(
                        x: transformer.fittedRect.midX,
                        y: transformer.fittedRect.midY
                    )
                    .clipped()

                // Virtual cursor overlay in Trackpad mode
                if touchMode == .trackpad {
                    VirtualCursorView(
                        position: virtualCursorPosition,
                        isPressed: isVirtualCursorPressed
                    )
                }

                // Interactive touch surface overlay
                touchGestureOverlay(in: geometry.size)

                // Top Quality & Status Bar
                VStack {
                    HStack {
                        ConnectionQualityBadge(
                            quality: controller.quality,
                            isCellular: lifecycleManager.isCellular
                        )
                        Spacer()

                        // Mode indicator pill
                        Text(touchMode.rawValue)
                            .font(.caption2.weight(.bold))
                            .padding(.horizontal, 8)
                            .padding(.vertical, 4)
                            .background(.ultraThinMaterial, in: Capsule())
                    }
                    .padding(.horizontal, 16)
                    .padding(.top, 8)

                    Spacer()

                    // Floating Input & Control Toolbar
                    InputToolbarView(
                        touchMode: $touchMode,
                        isKeyboardVisible: $isKeyboardVisible,
                        isModifierBarVisible: $isModifierBarVisible,
                        isQualityExpanded: $isQualityExpanded,
                        isMicEnabled: controller.isMicEnabled,
                        onToggleMic: handleToggleMic,
                        onSendModifierKey: handleSendModifierKey,
                        onSendText: { text in
                            controller.sendText(text)
                        },
                        onOpenFileTransfer: {
                            isDocumentPickerPresented = true
                        },
                        onDisconnect: {
                            controller.disconnect()
                            controller.close()
                            onExit()
                        }
                    )
                }
            }
            .onAppear {
                updateTransformer(viewSize: geometry.size)
            }
            .onChange(of: geometry.size) { newSize in
                updateTransformer(viewSize: newSize)
            }
            .onChange(of: controller.geometry) { newGeom in
                if newGeom.width > 0 && newGeom.height > 0 {
                    transformer.desktopWidth = newGeom.width
                    transformer.desktopHeight = newGeom.height
                }
            }
        }
        .statusBarHidden(true)
        .sheet(isPresented: $isDocumentPickerPresented) {
            DocumentPickerView { url in
                // Handle file upload within iOS document sandbox
            }
        }
    }

    // MARK: - Gestures

    @ViewBuilder
    private func touchGestureOverlay(in size: CGSize) -> some View {
        Color.clear
            .contentShape(Rectangle())
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { value in
                        handleDragChanged(value)
                    }
                    .onEnded { value in
                        handleDragEnded(value)
                    }
            )
            .simultaneousGesture(
                MagnificationGesture()
                    .onChanged { scale in
                        zoomScale = max(1.0, min(scale, 4.0))
                        transformer.zoomScale = zoomScale
                    }
            )
    }

    private func handleDragChanged(_ value: DragGesture.Value) {
        switch touchMode {
        case .directTouch:
            if let target = transformer.viewPointToDesktopPoint(value.location) {
                controller.sendPointer(
                    x: Int(target.x),
                    y: Int(target.y),
                    action: .move,
                    button: .primary
                )
            }

        case .trackpad:
            let delta = CGPoint(
                x: value.translation.width,
                y: value.translation.height
            )
            let updated = transformer.applyTrackpadDelta(
                currentDesktop: (x: Int32(virtualCursorPosition.x), y: Int32(virtualCursorPosition.y)),
                deltaView: delta
            )
            virtualCursorPosition = CGPoint(x: CGFloat(updated.x), y: CGFloat(updated.y))
            isVirtualCursorPressed = true

            controller.sendPointer(
                x: Int(updated.x),
                y: Int(updated.y),
                action: .move,
                button: .none
            )
        }
    }

    private func handleDragEnded(_ value: DragGesture.Value) {
        switch touchMode {
        case .directTouch:
            if let target = transformer.viewPointToDesktopPoint(value.location) {
                controller.sendPointer(
                    x: Int(target.x),
                    y: Int(target.y),
                    action: .up,
                    button: .primary
                )
            }

        case .trackpad:
            isVirtualCursorPressed = false
            let desktopPoint = (x: Int32(virtualCursorPosition.x), y: Int32(virtualCursorPosition.y))
            controller.sendPointer(
                x: Int(desktopPoint.x),
                y: Int(desktopPoint.y),
                action: .down,
                button: .primary
            )
            controller.sendPointer(
                x: Int(desktopPoint.x),
                y: Int(desktopPoint.y),
                action: .up,
                button: .primary
            )
        }
    }

    private func handleToggleMic() {
        if controller.isMicEnabled {
            controller.setMicEnabled(false)
        } else {
            AVAudioApplication.requestRecordPermission { granted in
                if granted {
                    Task { @MainActor in
                        controller.setMicEnabled(true)
                    }
                }
            }
        }
    }

    private func handleSendModifierKey(_ keyCode: UInt32) {
        controller.sendKey(keyCode: keyCode, action: .down)
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            controller.sendKey(keyCode: keyCode, action: .up)
        }
    }

    private func updateTransformer(viewSize: CGSize) {
        transformer.viewportSize = viewSize
        transformer.zoomScale = zoomScale
        transformer.panOffset = panOffset
    }
}

/// Simple document picker for sending files from iOS to remote workstation.
public struct DocumentPickerView: UIViewControllerRepresentable {
    public let onPick: (URL) -> Void

    public func makeUIViewController(context: Context) -> UIDocumentPickerViewController {
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.item])
        picker.delegate = context.coordinator
        picker.allowsMultipleSelection = false
        return picker
    }

    public func updateUIViewController(_ uiViewController: UIDocumentPickerViewController, context: Context) {}

    public func makeCoordinator() -> Coordinator {
        Coordinator(self)
    }

    public final class Coordinator: NSObject, UIDocumentPickerDelegate {
        let parent: DocumentPickerView

        init(_ parent: DocumentPickerView) {
            self.parent = parent
        }

        public func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
            if let first = urls.first {
                parent.onPick(first)
            }
        }
    }
}
