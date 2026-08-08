// Home — Threads are the durable corpus. Spaces are a searchable local
// filter and new-thread target rather than the mobile navigation spine.

import SwiftUI

enum Route: Hashable {
    case space(String)
    case chat(String)
    case newSession(spaceId: String)
}

struct HomeView: View {
    @Environment(AppModel.self) private var model
    @AppStorage("homeSpaceFilter") private var storedSpaceFilter = ""
    @AppStorage("lastNewSessionSpaceId") private var lastNewSessionSpaceId = ""
    @State private var path: [Route] = []
    @State private var showNewSpace = false
    @State private var showSpaceFilter = false
    @State private var sessionToDelete: Chat?

    private var deleteConfirmationPresented: Binding<Bool> {
        Binding(
            get: { sessionToDelete != nil },
            set: { if !$0 { sessionToDelete = nil } }
        )
    }

    private var spaceFilter: String? {
        storedSpaceFilter.isEmpty ? nil : storedSpaceFilter
    }

    private var filteredChats: [Chat] {
        guard let spaceFilter else { return model.overviewChats }
        return model.overviewChats.filter { $0.spaceId == spaceFilter }
    }

    private var filterLabel: String {
        guard let id = spaceFilter,
              let space = model.spaces.first(where: { $0.id == id }) else { return "All spaces" }
        return space.displayName
    }

    var body: some View {
        NavigationStack(path: $path) {
            List {
                filterRow
                sessionsSection
            }
            .listStyle(.plain)
            .environment(\.defaultMinListRowHeight, 10)
            .contentMargins(.top, 2, for: .scrollContent)
            .scrollContentBackground(.hidden)
            .scrollEdgeEffectStyle(.soft, for: .top)
            .background(Theme.surface.ignoresSafeArea())
            .confirmationDialog(
                "Delete thread?",
                isPresented: deleteConfirmationPresented,
                titleVisibility: .visible,
                presenting: sessionToDelete
            ) { chat in
                Button("Delete", role: .destructive) {
                    model.deleteChat(chat.id)
                }
                Button("Cancel", role: .cancel) {}
            } message: { chat in
                Text("“\(chat.displayTitle)” will be permanently deleted. This can’t be undone.")
            }
            .navigationTitle("Jolt")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar(removing: .title)
            .navigationDestination(for: Route.self) { route in
                switch route {
                case .space(let id): SpaceView(spaceId: id, path: $path)
                case .chat(let id): SessionView(chatId: id)
                case .newSession(let spaceId): NewSessionView(spaceId: spaceId, path: $path)
                }
            }
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    if !model.connected {
                        ProgressView()
                            .controlSize(.mini)
                            .tint(Theme.textMuted)
                            .accessibilityLabel("Connecting")
                    }
                }
                .sharedBackgroundVisibility(.hidden)
                ToolbarItem(placement: .topBarTrailing) {
                    Button(action: openNewSession) {
                        TablerIconView(.plus, size: 16)
                    }
                    .accessibilityLabel("New thread")
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Menu {
                        Button {
                            showNewSpace = true
                        } label: {
                            TablerLabel("New space", icon: .folderPlus)
                        }
                        if model.demo != nil {
                            Text("Demo mode")
                        }
                        Button(role: .destructive) {
                            model.signOut()
                        } label: {
                            TablerLabel("Sign out", icon: .logout)
                        }
                    } label: {
                        TablerIconView(.userCircle, size: 16)
                    }
                }
            }
            .sheet(isPresented: $showSpaceFilter) {
                SpaceFilterSheet(selected: spaceFilter) { selected in
                    storedSpaceFilter = selected ?? ""
                }
            }
            .sheet(isPresented: $showNewSpace) {
                NewSpaceSheet { spaceId in
                    storedSpaceFilter = spaceId
                    lastNewSessionSpaceId = spaceId
                    path.append(.space(spaceId))
                }
            }
            .task(id: model.overviewChats.map(\.id).joined()) {
                model.preloadSessions()
            }
            .onChange(of: model.spaces.map(\.id)) { _, ids in
                if let filter = spaceFilter, !ids.contains(filter) {
                    storedSpaceFilter = ""
                }
            }
            .onAppear {
                if let route = model.launchRoute {
                    model.launchRoute = nil
                    if case .space(let id) = route, model.launchSheet == "newsession" {
                        model.launchSheet = nil
                        path = [route, .newSession(spaceId: id)]
                    } else {
                        path = [route]
                    }
                }
                if model.launchSheet == "newspace" {
                    model.launchSheet = nil
                    showNewSpace = true
                }
            }
        }
    }

    private var filterRow: some View {
        Button {
            showSpaceFilter = true
        } label: {
            HStack(spacing: 8) {
                TablerIconView(.folder, size: 13)
                    .foregroundStyle(Theme.textMuted)
                Text(filterLabel)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                Spacer(minLength: 8)
                TablerIconView(.selector, size: 9)
                    .foregroundStyle(Theme.textFaint)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 8)
            .contentShape(RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(PressWashButtonStyle())
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
        .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 2, trailing: 12))
    }

    private var sessionsSection: some View {
        Section {
            Text("Threads")
                .font(Theme.sans(11, weight: .medium))
                .foregroundStyle(Theme.textMuted.opacity(0.6))
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 3, trailing: 16))

            let chats = filteredChats
            if chats.isEmpty {
                Text(spaceFilter == nil ? "No threads yet" : "No threads in this space")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textFaint)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            let firstUnpinnedId = chats.firstIndex(where: { !$0.pinned })
                .flatMap { $0 > 0 ? chats[$0].id : nil }
            ForEach(chats) { chat in
                Button {
                    path.append(.chat(chat.id))
                } label: {
                    ChatRow(chat: chat, showLocation: true)
                }
                .buttonStyle(PressWashButtonStyle())
                .overlay(alignment: .top) {
                    if chat.id == firstUnpinnedId {
                        Rectangle()
                            .fill(Theme.textFaint.opacity(0.35))
                            .frame(height: 0.5)
                            .allowsHitTesting(false)
                    }
                }
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
                .swipeActions(edge: .leading, allowsFullSwipe: true) {
                    Button {
                        sessionToDelete = chat
                    } label: {
                        TablerLabel("Delete", icon: .trash)
                    }
                    // The confirmation button owns the destructive role. Giving
                    // it to this trigger makes List optimistically remove the row.
                    .tint(Theme.danger)
                }
                .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                    Button {
                        model.setPinned(chatId: chat.id, pinned: !chat.pinned)
                    } label: {
                        TablerLabel(chat.pinned ? "Unpin" : "Pin", icon: .pin)
                    }
                    .tint(Theme.textMuted)
                    Button {
                        model.archive(chatId: chat.id)
                    } label: {
                        TablerLabel("Close", icon: .messageCircleX)
                    }
                    .tint(Theme.surfaceRaised)
                    .disabled(model.indicator(for: chat) == .working
                              || model.indicator(for: chat) == .awaitingInput)
                }
            }
            .motionAnimation(Motion.resort, value: chats.map(\.id))
        }
    }

    private func openNewSession() {
        guard let target = resolvedNewSessionSpace() else {
            showNewSpace = true
            return
        }
        lastNewSessionSpaceId = target
        path.append(.newSession(spaceId: target))
    }

    private func resolvedNewSessionSpace() -> String? {
        let ids = Set(model.spaces.map(\.id))
        if let filter = spaceFilter, ids.contains(filter) { return filter }
        if ids.contains(lastNewSessionSpaceId) { return lastNewSessionSpaceId }
        return model.spaces.first?.id
    }
}

struct SpaceFilterSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let selected: String?
    var includeAll = true
    var title = "Filter threads"
    let onSelect: (String?) -> Void
    @State private var query = ""

    private var spaces: [Space] {
        let sorted = model.spaces.sorted {
            let order = $0.displayName.localizedCaseInsensitiveCompare($1.displayName)
            return order == .orderedSame ? $0.id < $1.id : order == .orderedAscending
        }
        guard !query.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return sorted }
        return sorted.filter { $0.displayName.localizedCaseInsensitiveContains(query) }
    }

    var body: some View {
        NavigationStack {
            List {
                if includeAll, query.isEmpty {
                    row(label: "All spaces", detail: nil, id: nil)
                }
                ForEach(spaces) { space in
                    let online = model.deviceOnline(space.deviceId)
                    row(label: space.displayName,
                        detail: online
                            ? "@ \(model.deviceName(space.deviceId))"
                            : "@ \(model.deviceName(space.deviceId)) · offline",
                        id: space.id)
                }
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
            .background(Theme.surface.ignoresSafeArea())
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .searchable(text: $query, prompt: "Search spaces")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
        .preferredColorScheme(.dark)
    }

    private func row(label: String, detail: String?, id: String?) -> some View {
        let isSelected = selected == id
        return Button {
            onSelect(id)
            dismiss()
        } label: {
            HStack(spacing: 10) {
                TablerIconView(.folder)
                    .foregroundStyle(Theme.textMuted)
                Text(label)
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                Spacer(minLength: 8)
                if let detail {
                    Text(detail)
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.textMuted.opacity(0.6))
                        .lineLimit(1)
                }
                SheetSelectionIndicator(selected: isSelected)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle(selected: isSelected))
        .listRowBackground(Color.clear)
    }
}

// MARK: - Rows

/// The desktop session row (shell.rs `render_chat_row`), line for line: the
/// status rail leads a muted context line carrying the space name and the
/// relative time; the title sits on its own line below; harness mark and branch
/// close it out. Lines 2 and 3 indent by rail + gap so they start exactly under
/// the context line rather than beside the rail.
///
/// The one addition the phone needs: the desktop row names only the space
/// because its sidebar sits on the machine running the work. Here the Threads
/// list interleaves every device, and a thread whose host has gone offline
/// can't be driven at all — so the context line reads "space @ device".
struct ChatRow: View {
    @Environment(AppModel.self) private var model
    let chat: Chat
    var showLocation: Bool

    /// Rail (6) + gap (8) — see `render_chat_row`'s `pl(px(14.0))`.
    private static let indent: CGFloat = StatusRail.width + 8

    private var subline: Color { Theme.textMuted.opacity(0.5) }

    var body: some View {
        let indicator = model.indicator(for: chat)
        VStack(alignment: .leading, spacing: 2) {
            // Line 1: status rail, space @ device, time-ago.
            HStack(spacing: 8) {
                StatusRail(indicator: indicator)
                if showLocation {
                    Text(location)
                        .font(Theme.sans(11))
                        .foregroundStyle(subline)
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Spacer(minLength: 4)
                }
                if chat.pinned {
                    TablerIconView(.pin, size: 10, color: subline)
                        .accessibilityLabel("Pinned")
                }
                Text(relativeTime(chat.lastMessageAt ?? chat.createdAt))
                    .font(Theme.sans(11))
                    .foregroundStyle(subline)
                    .fixedSize()
            }

            // Line 2: the thread title.
            Text(chat.displayTitle)
                .font(Theme.sans(13))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.leading, Self.indent)

            // Line 3: harness brand mark, then the branch when the engine
            // stamped one.
            HStack(spacing: 4) {
                if let harness = chat.config?.harness {
                    HarnessBadge(harness: harness, size: 11, neutral: subline)
                }
                if let branch = chat.branch?.trimmingCharacters(in: .whitespaces), !branch.isEmpty {
                    TablerIconView(.gitBranch, size: 11, color: subline)
                    Text(branch)
                        .font(Theme.sans(11))
                        .foregroundStyle(subline)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                Spacer(minLength: 0)
            }
            .padding(.leading, Self.indent)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }

    /// "space @ device", with offline marker. The space name (not the cwd
    /// basename) is what the desktop row shows — they differ once a space has
    /// been renamed, or when the session runs in a worktree off to the side.
    private var location: String {
        let space = model.space(for: chat)?.displayName
            ?? chat.cwd.map { ($0 as NSString).lastPathComponent }
            ?? "?"
        let name = model.deviceName(chat.deviceId)
        return model.deviceOnline(chat.deviceId)
            ? "\(space) @ \(name)"
            : "\(space) @ \(name) (offline)"
    }
}

func relativeTime(_ ms: Int64) -> String {
    let delta = max(0, nowMs() - ms) / 1000
    if delta < 60 { return "now" }
    if delta < 3600 { return "\(delta / 60)m" }
    if delta < 86_400 { return "\(delta / 3600)h" }
    return "\(delta / 86_400)d"
}
