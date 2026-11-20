import SwiftUI
import UIKit

/// Receive-only controls on the original session. Stopping this direction
/// leaves desktop control alive and never discards a publication receipt.
struct FileDownloadBar: View {
    @ObservedObject var session: RemoteSession
    @State private var showFiles = false
    @State private var showReceipts = false

    var body: some View {
        if session.receivesFiles {
            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    Image(systemName: "tray.and.arrow.down")
                    Text(session.downloadStatus?.title ?? "Waiting for receive status")
                        .font(.caption.weight(.semibold))
                    Spacer(minLength: 6)
                    if session.downloadStatus?.canCancel == true {
                        Button("Stop receiving") { session.stopReceivingFiles() }
                            .font(.caption)
                            .disabled(!session.downloadStatusCurrent)
                    }
                }
                if let status = session.downloadStatus {
                    if let progress = status.progress {
                        if let fraction = status.progressFraction { ProgressView(value: fraction) }
                        Text("\(FileUploadStatus.Transfer.bytes(progress.stagedBytes)) of \(FileUploadStatus.Transfer.bytes(progress.totalBytes)) staged — awaiting a publication receipt")
                            .font(.caption2).foregroundStyle(.secondary)
                    }
                    if !status.receipts.isEmpty {
                        DisclosureGroup("\(status.receipts.count) file result\(status.receipts.count == 1 ? "" : "s")",
                                        isExpanded: $showReceipts) {
                            ScrollView {
                                VStack(alignment: .leading, spacing: 5) {
                                    ForEach(status.receipts) { receipt in
                                        Text("File \(receipt.index.map { String($0 + 1) } ?? "?"): \(receipt.title)")
                                            .font(.caption2)
                                    }
                                }.frame(maxWidth: .infinity, alignment: .leading)
                            }.frame(maxHeight: 110)
                        }.font(.caption)
                    }
                    if status.receiptsTruncated {
                        Text("Some results were omitted; this is not a complete success report.")
                            .font(.caption2).foregroundStyle(.orange)
                    }
                }
                if session.downloadCleanupComplete {
                    Button("Show received files…") {
                        session.stopClipboard()
                        showFiles = true
                    }
                        .font(.caption)
                } else {
                    Text("Files stay in FrankenRemote. Disconnect and finish cleanup before sharing a saved file.")
                        .font(.caption2).foregroundStyle(.secondary)
                }
                if !session.downloadStatusCurrent {
                    Text("Current receive status is unavailable; earlier receipts are retained.")
                        .font(.caption2).foregroundStyle(.orange)
                }
            }
            .padding(10)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
            .environment(\.colorScheme, .dark)
            .sheet(isPresented: $showFiles) { ReceivedFilesView() }
        }
    }
}

/// An explicit local export UI. No view preview, automatic opening, remote
/// listing or new receive permission is implied by opening this sheet.
struct ReceivedFilesView: View {
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase
    @State private var listing = ReceivedFiles()
    @State private var loading = true
    @State private var exportInProgress = false
    @State private var export: FileExport?
    @State private var notice: String?
    @State private var exportTask: Task<Void, Never>?

    var body: some View {
        NavigationStack {
            List {
                Section {
                    if loading { ProgressView("Reading saved files…") }
                    else if listing.files.isEmpty {
                        Text("No completed files are available to share.")
                            .foregroundStyle(.secondary)
                        Text("To receive files, touch and hold a machine, then choose Connect and receive files. After disconnecting, pull to refresh when cleanup finishes.")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                    ForEach(listing.files) { file in
                        Button {
                            share(file)
                        } label: {
                            HStack {
                                Image(systemName: "doc")
                                VStack(alignment: .leading) {
                                    Text(file.name).lineLimit(2)
                                    Text(ByteCountFormatter.string(fromByteCount: Int64(file.bytes), countStyle: .file))
                                        .font(.caption).foregroundStyle(.secondary)
                                }
                                Spacer()
                                Image(systemName: "square.and.arrow.up")
                            }
                        }
                        .disabled(exportInProgress)
                        .accessibilityLabel("Share \(file.name)")
                    }
                } footer: {
                    Text("These are local files saved in FrankenRemote. Tap one to choose where to copy or share it. Active receivers and private staging files are excluded. Nothing opens automatically.")
                }
                if listing.truncated {
                    Section { Text("This bounded listing is incomplete. Some saved files are not shown.").foregroundStyle(.orange) }
                }
                if listing.unavailable {
                    Section { Text("Saved files could not be read. Unlock this device and try again.").foregroundStyle(.orange) }
                }
                if let notice { Section { Text(notice).foregroundStyle(.orange) } }
            }
            .navigationTitle("Received files")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
            .task { await refresh() }
            .refreshable { await refresh() }
            .sheet(item: $export) { item in FileShareSheet(url: item.url) }
            .onDisappear { exportTask?.cancel() }
            .onChange(of: scenePhase) { _, phase in
                if phase != .active { exportTask?.cancel() }
            }
        }
    }

    @MainActor
    private func refresh() async {
        guard !exportInProgress else { return }
        loading = true
        let next = await Task.detached(priority: .utility) { FileDownloadResource.catalog() }.value
        guard !Task.isCancelled else { return }
        listing = next
        loading = false
    }

    @MainActor
    private func share(_ file: ReceivedFile) {
        guard !exportInProgress else { return }
        exportInProgress = true
        notice = nil
        exportTask = Task { @MainActor in
            let url = await Task.detached(priority: .userInitiated) {
                FileDownloadResource.exportURL(file)
            }.value
            exportInProgress = false
            exportTask = nil
            guard !Task.isCancelled, scenePhase == .active else { return }
            if let url { export = FileExport(url: url) }
            else { notice = "That file is no longer available for sharing. Refresh the saved-file list." }
        }
    }
}

private struct FileExport: Identifiable {
    let id = UUID()
    let url: URL
}

private struct FileShareSheet: UIViewControllerRepresentable {
    let url: URL
    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: [url], applicationActivities: nil)
    }
    func updateUIViewController(_ controller: UIActivityViewController, context: Context) {}
}
