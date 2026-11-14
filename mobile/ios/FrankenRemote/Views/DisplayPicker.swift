import SwiftUI

/// A choice from one complete, approved catalog. Each action closes over
/// that original request, including the sheet's interactive dismissal.
struct DisplayPicker: View {
    let host: String
    let selection: DisplaySelection
    let choose: (UInt32) -> Void
    let cancel: () -> Void

    var body: some View {
        NavigationStack {
            List {
                Section {
                    ForEach(selection.displays) { display in
                        Button {
                            choose(display.id)
                        } label: {
                            HStack(spacing: 14) {
                                Image(systemName: "display").font(.title2)
                                    .accessibilityHidden(true)
                                VStack(alignment: .leading, spacing: 4) {
                                    Text(display.title).font(.headline)
                                    Text(display.pixels).font(.subheadline)
                                    Text(display.placement).font(.caption)
                                    Text(display.details).font(.caption)
                                }
                                .foregroundStyle(.primary)
                                Spacer(minLength: 0)
                                Image(systemName: "chevron.right")
                                    .font(.caption.weight(.semibold))
                                    .foregroundStyle(.secondary)
                                    .accessibilityHidden(true)
                            }
                            .padding(.vertical, 6)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .contentShape(Rectangle())
                        }
                        .accessibilityElement(children: .combine)
                    }
                } header: {
                    Text(host)
                } footer: {
                    Text("Only displays approved for this session are listed. Cancelling closes this connection.")
                }
            }
            .navigationTitle("Choose a display")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: cancel)
                }
            }
        }
    }
}
