import SwiftUI
import UniformTypeIdentifiers
import UIKit

/// The system's single-document chooser. It selects access to a resource;
/// it never sends a file or grants control. Each delegate consumes its one
/// callback, including cancellation or a malformed multi-selection.
struct FileUploadPicker: UIViewControllerRepresentable {
    let completed: (URL?) -> Void

    func makeCoordinator() -> Coordinator { Coordinator(completed: completed) }

    func makeUIViewController(context: Context) -> UIDocumentPickerViewController {
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.item], asCopy: false)
        picker.allowsMultipleSelection = false
        picker.shouldShowFileExtensions = true
        picker.delegate = context.coordinator
        return picker
    }

    func updateUIViewController(_ controller: UIDocumentPickerViewController, context: Context) {}

    @MainActor
    final class Coordinator: NSObject, UIDocumentPickerDelegate {
        private var completed: ((URL?) -> Void)?

        init(completed: @escaping (URL?) -> Void) { self.completed = completed }

        func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
            finish(urls.count == 1 && urls[0].isFileURL ? urls[0] : nil)
        }

        func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
            finish(nil)
        }

        private func finish(_ url: URL?) {
            let callback = completed
            completed = nil
            callback?(url)
        }
    }
}

/// Bounded status over the still-visible desktop. A separate covering sheet
/// would end control, so the fresh Send and current-batch commands stay here.
struct FileUploadBar: View {
    @ObservedObject var session: RemoteSession

    var body: some View {
        if let choice = session.uploadChoice {
            VStack(alignment: .leading, spacing: 5) {
                HStack {
                    Label(choice.name ?? "Choose one file", systemImage: "doc")
                        .font(.subheadline.weight(.semibold)).lineLimit(1).truncationMode(.middle)
                    Spacer(minLength: 6)
                    Button("Discard", role: .cancel) {
                        session.discardUploadSelection(token: choice.token)
                    }
                    .font(.caption)
                }
                if let message = session.uploadStatus?.transfer?.unavailableMessage {
                    Text(message).font(.caption).foregroundStyle(.orange)
                    Text("No file was sent. Discard this selection or open another session.")
                        .font(.caption2).foregroundStyle(.secondary)
                } else if choice.phase == .preparing {
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.small)
                        Text("Preparing local upload copy…").font(.caption)
                    }
                    Text("Your provider may download the file. Nothing is sent yet.")
                        .font(.caption2).foregroundStyle(.secondary)
                } else if choice.phase == .prepared {
                    HStack(spacing: 10) {
                        Text(session.phase == .controlled
                             ? "Send when this view and the file channel are ready."
                             : "Request Control, then send this file once.")
                            .font(.caption).foregroundStyle(.secondary)
                        Spacer(minLength: 0)
                        Button("Send once") { session.sendUploadOnce() }
                            .buttonStyle(.borderedProminent)
                            .disabled(!session.canSendUpload)
                    }
                } else {
                    Text("Creates a local copy, up to 256 MiB. Sending is a separate action.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            .padding(10)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
            .frame(maxWidth: 440)
        } else if let transfer = session.uploadStatus?.transfer, transfer.batch > 0 {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 10) {
                    Text(transfer.title).font(.caption.weight(.semibold)).lineLimit(2)
                    Spacer(minLength: 0)
                    if transfer.canPause {
                        Button(transfer.pauseRequested ? "Resume" : "Pause") {
                            session.pauseUpload(batch: transfer.batch, paused: !transfer.pauseRequested)
                        }
                        .disabled(!session.uploadStatusCurrent)
                    }
                    if transfer.canCancel {
                        Button("Cancel", role: .destructive) { session.cancelUpload(batch: transfer.batch) }
                            .disabled(!session.uploadStatusCurrent)
                    }
                }
                .font(.caption)
                if !transfer.complete, let progress = transfer.progress {
                    ProgressView(value: progress)
                    if let text = transfer.progressText {
                        Text(text).font(.caption2).foregroundStyle(.secondary)
                    }
                }
                if let history = session.uploadStatus?.history, !history.isEmpty {
                    let received = history.filter { $0.receipt == "host_published" }.count
                    Text("Uploads: \(received) host receipts, \(history.count - received) refused or unconfirmed.")
                        .font(.caption2).foregroundStyle(.secondary).lineLimit(2)
                }
            }
            .padding(10)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
            .frame(maxWidth: 440)
        } else if let message = session.uploadStatus?.transfer?.unavailableMessage {
            VStack(alignment: .leading, spacing: 3) {
                Label(message, systemImage: "doc")
                    .font(.caption.weight(.semibold))
                Text("No file was sent.")
                    .font(.caption2).foregroundStyle(.secondary)
            }
            .padding(10)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
            .frame(maxWidth: 440)
        }
    }
}
