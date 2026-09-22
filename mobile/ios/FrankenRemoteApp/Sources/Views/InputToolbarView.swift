//
// InputToolbarView.swift — Discoverable Input & Control HUD
//

import SwiftUI
import FrankenRemoteKit

public struct InputToolbarView: View {
    @Binding public var touchMode: TouchMode
    @Binding public var isKeyboardVisible: Bool
    @Binding public var isModifierBarVisible: Bool
    @Binding public var isQualityExpanded: Bool

    public let isMicEnabled: Bool
    public let onToggleMic: () -> Void
    public let onSendModifierKey: (UInt32) -> Void
    public let onSendText: (String) -> Void
    public let onOpenFileTransfer: () -> Void
    public let onDisconnect: () -> Void

    @State private var textInputBuffer: String = ""
    @FocusState private var isTextFieldFocused: Bool

    public init(
        touchMode: Binding<TouchMode>,
        isKeyboardVisible: Binding<Bool>,
        isModifierBarVisible: Binding<Bool>,
        isQualityExpanded: Binding<Bool>,
        isMicEnabled: Bool,
        onToggleMic: @escaping () -> Void,
        onSendModifierKey: @escaping (UInt32) -> Void,
        onSendText: @escaping (String) -> Void,
        onOpenFileTransfer: @escaping () -> Void,
        onDisconnect: @escaping () -> Void
    ) {
        self._touchMode = touchMode
        self._isKeyboardVisible = isKeyboardVisible
        self._isModifierBarVisible = isModifierBarVisible
        self._isQualityExpanded = isQualityExpanded
        self.isMicEnabled = isMicEnabled
        self.onToggleMic = onToggleMic
        self.onSendModifierKey = onSendModifierKey
        self.onSendText = onSendText
        self.onOpenFileTransfer = onOpenFileTransfer
        self.onDisconnect = onDisconnect
    }

    public var body: some View {
        VStack(spacing: 8) {
            // Modifier keys bar (Cmd, Ctrl, Alt, Shift, Esc, Tab, Enter)
            if isModifierBarVisible {
                modifierKeysRow
            }

            // Hidden or compact text field for software keyboard & IME support
            if isKeyboardVisible {
                textEntryRow
            }

            // Main floating HUD bar
            mainControlsRow
        }
        .padding(.horizontal, 16)
        .padding(.bottom, 8)
    }

    private var mainControlsRow: some View {
        HStack(spacing: 12) {
            // Touch mode switch
            Button {
                touchMode = (touchMode == .directTouch) ? .trackpad : .directTouch
            } label: {
                Label(touchMode.rawValue, systemImage: touchMode.systemImage)
                    .font(.caption.weight(.medium))
            }
            .buttonStyle(.borderedProminent)
            .tint(touchMode == .trackpad ? .indigo : .blue)

            // Keyboard toggle
            Button {
                isKeyboardVisible.toggle()
                if isKeyboardVisible {
                    isTextFieldFocused = true
                } else {
                    isTextFieldFocused = false
                }
            } label: {
                Image(systemName: "keyboard")
            }
            .buttonStyle(.bordered)

            // Modifier keys toggle
            Button {
                isModifierBarVisible.toggle()
            } label: {
                Image(systemName: "command")
            }
            .buttonStyle(.bordered)
            .tint(isModifierBarVisible ? .accentColor : .primary)

            // Talk toggle (Mic uplink)
            Button {
                onToggleMic()
            } label: {
                Image(systemName: isMicEnabled ? "mic.fill" : "mic.slash")
            }
            .buttonStyle(.bordered)
            .tint(isMicEnabled ? .green : .secondary)

            // File transfer
            Button {
                onOpenFileTransfer()
            } label: {
                Image(systemName: "folder")
            }
            .buttonStyle(.bordered)

            Spacer()

            // Disconnect
            Button(role: .destructive) {
                onDisconnect()
            } label: {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(.red)
            }
        }
        .padding(8)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 16))
    }

    private var modifierKeysRow: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                modifierButton("Esc", keyCode: 0x35)
                modifierButton("Tab", keyCode: 0x30)
                modifierButton("⌃ Ctrl", keyCode: 0x3B)
                modifierButton("⌥ Opt", keyCode: 0x3A)
                modifierButton("⌘ Cmd", keyCode: 0x37)
                modifierButton("⇧ Shift", keyCode: 0x38)
                modifierButton("⌫ Del", keyCode: 0x33)
                modifierButton("↩ Enter", keyCode: 0x24)
            }
            .padding(.horizontal, 4)
        }
        .padding(6)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    private func modifierButton(_ title: String, keyCode: UInt32) -> some View {
        Button(title) {
            onSendModifierKey(keyCode)
        }
        .font(.caption2.weight(.semibold).monospaced())
        .buttonStyle(.bordered)
        .controlSize(.small)
    }

    private var textEntryRow: some View {
        HStack {
            TextField("Type text to send to host...", text: $textInputBuffer)
                .textFieldStyle(.roundedBorder)
                .focused($isTextFieldFocused)
                .onSubmit {
                    commitText()
                }

            Button("Send") {
                commitText()
            }
            .buttonStyle(.borderedProminent)
            .disabled(textInputBuffer.isEmpty)
        }
        .padding(8)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    private func commitText() {
        guard !textInputBuffer.isEmpty else { return }
        onSendText(textInputBuffer)
        textInputBuffer = ""
    }
}
