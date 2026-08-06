// New session — a composer page rather than a form. Its canvas uses a faded
// mark, prompt, and glass composer with in-pill picker chips. The space already
// fixes device + folder; the composer
// carries the agent/model chip, and sending mints the chat, queues the first
// run, and swaps straight into the live session.

import PhotosUI
import SwiftUI

struct NewSessionView: View {
    @Environment(AppModel.self) private var model
    let spaceId: String
    @Binding var path: [Route]
    @State private var selectedSpaceId: String

    init(spaceId: String, path: Binding<[Route]>) {
        self.spaceId = spaceId
        _path = path
        _selectedSpaceId = State(initialValue: spaceId)
    }

    // Sticky run configuration.
    @AppStorage("newSessionHarness") private var harness = "claude-code"
    @AppStorage("newSessionModel") private var storedModel = ""
    @AppStorage("newSessionReasoning") private var storedReasoning = ""
    @AppStorage("lastNewSessionSpaceId") private var lastNewSessionSpaceId = ""

    @State private var draft = ""
    @State private var selection: TextSelection?
    @State private var mentions = FileMentionDraft()
    @State private var showPicker = false
    @State private var showSpacePicker = false
    @State private var showRefPicker = false
    @State private var showCheckoutPicker = false
    /// Live per-harness catalogs from the space's device (static fallback).
    @State private var catalogs: [String: [ModelInfo]] = [:]
    @State private var harnesses = HarnessCatalog.harnesses
    @State private var refs: [RepoRef] = []
    @State private var selectedRef: String?
    @State private var checkoutKind: CheckoutKind = .local
    @State private var busy = false
    @State private var attachments: [StagedAttachment] = []
    @State private var pickerItems: [PhotosPickerItem] = []
    @State private var showPhotoPicker = false
    @State private var showGoalSheet = false
    @State private var sendError: String?
    @FocusState private var focused: Bool

    private var space: Space? {
        model.spaces.first { $0.id == selectedSpaceId }
    }

    private var models: [ModelInfo] {
        catalogs[harness] ?? HarnessCatalog.models(for: harness)
    }

    private var selectedModel: ModelInfo? {
        models.first { $0.id == storedModel } ?? models.first
    }

    private var reasoning: String? {
        guard let selectedModel else { return nil }
        if selectedModel.reasoningLevels.isEmpty { return nil }
        if selectedModel.reasoningLevels.contains(storedReasoning) { return storedReasoning }
        return HarnessCatalog.defaultReasoning(for: selectedModel)
    }

    var body: some View {
        VStack(spacing: 0) {
            // Tapping the canvas dismisses the keyboard.
            ZStack {
                Theme.bg
                VStack(spacing: 24) {
                    JoltMark()
                        .frame(width: 84, height: 84)
                        .opacity(0.22)
                    Text("What are we building?")
                        .font(Theme.sans(15))
                        .foregroundStyle(Theme.textFaint)
                }
            }
            .contentShape(Rectangle())
            .onTapGesture { focused = false }

            if let space, !model.deviceOnline(space.deviceId), model.demo == nil {
                offlineNotice(space: space)
            }
            if let sendError {
                Text(sendError)
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.danger)
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 24)
                    .padding(.bottom, 6)
            }

            composer
                .padding(.bottom, 8)
        }
        .background(Theme.bg.ignoresSafeArea())
        .navigationTitle("New session")  // feeds the back menu
        .navigationBarTitleDisplayMode(.inline)
        .sheet(isPresented: $showGoalSheet) {
            NewGoalSheet(onCreate: createGoalSession)
        }
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: 1) {
                    Text("New session")
                        .font(Theme.sans(13, weight: .medium))
                        .foregroundStyle(Theme.text)
                    if let space {
                        Text("\(space.displayName) · \(model.deviceName(space.deviceId))")
                            .font(Theme.sans(10.5))
                            .foregroundStyle(Theme.textMuted.opacity(0.6))
                            .lineLimit(1)
                    }
                }
            }
        }
        .sheet(isPresented: $showSpacePicker) {
            SpaceFilterSheet(
                selected: selectedSpaceId,
                includeAll: false,
                title: "Session space"
            ) { selected in
                if let selected {
                    selectedSpaceId = selected
                }
            }
        }
        .sheet(isPresented: $showRefPicker) {
            RefPickerSheet(refs: refs, selected: selectedRef) { ref in
                await pickRef(ref)
            }
        }
        .sheet(isPresented: $showCheckoutPicker) {
            CheckoutPickerSheet(kind: checkoutKind,
                                selectedRefHasWorktree: selectedRefRow?.worktreePath != nil,
                                isJujutsu: isJujutsu) { kind in
                pickCheckout(kind)
            }
        }
        .task(id: selectedSpaceId) {
            guard let space else { return }
            let requestedSpace = space.id
            let loadedHarnesses = await model.listHarnesses(space: space)
            guard selectedSpaceId == requestedSpace else { return }
            harnesses = loadedHarnesses
            if !harnesses.contains(where: { $0.id == harness }) {
                harness = harnesses.first?.id ?? "claude-code"
                storedModel = ""
                storedReasoning = ""
            }
            // Load refs for any VCS recognized by the host's active backend.
            guard space.gitDetected else { return }
            if let loaded = await model.listRefs(space: space) {
                guard selectedSpaceId == requestedSpace else { return }
                refs = loaded
                if selectedRef == nil {
                    selectedRef = loaded.first(where: \.current)?.id ?? loaded.first?.id
                }
            }
        }
        .task(id: "\(selectedSpaceId)/\(harness)") {
            // Live model catalog from the device that will run the session.
            guard let space else { return }
            let requestedSpace = space.id
            let requestedHarness = harness
            let loaded = await model.listModels(space: space, harness: requestedHarness)
            guard selectedSpaceId == requestedSpace, harness == requestedHarness else { return }
            catalogs[requestedHarness] = loaded
            if !loaded.contains(where: { $0.id == storedModel }) {
                storedModel = loaded.first?.id ?? ""
                storedReasoning = loaded.first.flatMap(HarnessCatalog.defaultReasoning) ?? ""
            }
        }
        .sheet(isPresented: $showPicker) {
            ModelPickerSheet(harness: $harness, modelId: Binding(
                get: { selectedModel?.id ?? "" },
                set: { storedModel = $0 }
            ), reasoning: Binding(
                get: { reasoning },
                set: { storedReasoning = $0 ?? "" }
            ), catalogs: catalogs, harnesses: harnesses)
        }
        .photosPicker(isPresented: $showPhotoPicker, selection: $pickerItems,
                      maxSelectionCount: 8, matching: .images)
        .onChange(of: pickerItems) { _, items in
            guard !items.isEmpty else { return }
            stage(items)
        }
        .onChange(of: draft) { refreshMentions() }
        .onChange(of: selection) { refreshMentions() }
        .onChange(of: mentionContextKey) { refreshMentions() }
        .onChange(of: selectedSpaceId) { _, selected in
            lastNewSessionSpaceId = selected
            refs = []
            selectedRef = nil
            checkoutKind = .local
            catalogs.removeAll()
            sendError = nil
        }
        .onAppear {
            lastNewSessionSpaceId = selectedSpaceId
            focused = true
            if model.launchAutosend {
                model.launchAutosend = false
                draft = "Sketch the plan for porting the diff pane."
                Task { @MainActor in
                    try? await Task.sleep(nanoseconds: 800_000_000)
                    send()
                }
            }
        }
    }

    // MARK: Composer

    private var composer: some View {
        VStack(spacing: 6) {
            FileMentionMenu(draft: mentions, select: acceptMention)
            ComposerShell(
                draft: $draft,
                selection: $selection,
                placeholder: "Do anything…",
                sendEnabled: canSend,
                showStop: false,
                busy: busy,
                onSend: send,
                attachments: attachments,
                onAttach: { showPhotoPicker = true },
                onRemoveAttachment: { id in attachments.removeAll { $0.id == id } }
            ) {
            if let space {
                chip(
                    icon: .folder,
                    label: "\(space.displayName) @ \(model.deviceName(space.deviceId))"
                ) {
                    focused = false
                    showSpacePicker = true
                }
                .layoutPriority(1)
            }

            // Agent chip — brand mark + model, opens the picker sheet
            // (desktop's in-pill HarnessModel trigger chip).
            Button {
                focused = false
                showPicker = true
            } label: {
                HStack(spacing: 6) {
                    HarnessBadge(harness: harness, size: 15)
                    Text(selectedModel?.label ?? (harness == "pi" ? "Pi unavailable" : "Select model"))
                        .font(Theme.sans(13, weight: .medium))
                        .foregroundStyle(Theme.text.opacity(0.9))
                        .lineLimit(1)
                    if let reasoning {
                        Text(HarnessCatalog.reasoningLabel(reasoning))
                            .font(Theme.sans(12))
                            .foregroundStyle(Theme.textMuted)
                    }
                    Image(systemName: "chevron.up.chevron.down")
                        .font(.system(size: 9, weight: .medium))
                        .foregroundStyle(Theme.textFaint)
                }
                .padding(.horizontal, 13)
                .frame(height: 36)
                .background(whiteAlpha(0.10), in: Capsule())
            }
            .buttonStyle(ChipPressButtonStyle())

            // Checkout + ref chips from the host's active VCS backend.
            if space?.gitDetected == true {
                chip(icon: checkoutIcon, label: checkoutLabel) {
                    focused = false
                    showCheckoutPicker = true
                }
                .layoutPriority(-1)
                chip(icon: .gitBranch, label: refLabel) {
                    focused = false
                    showRefPicker = true
                }
                .layoutPriority(-1)
            }
            }
        }
    }

    private func chip(icon: LineIcon, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 6) {
                LineIconView(icon, size: 13, color: Theme.textMuted)
                Text(label)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.text.opacity(0.9))
                    .lineLimit(1)
            }
            .padding(.horizontal, 12)
            .frame(height: 36)
            .background(whiteAlpha(0.10), in: Capsule())
        }
        .buttonStyle(ChipPressButtonStyle())
    }

    // MARK: Checkout model (pickers.rs port)

    private var selectedRefRow: RepoRef? {
        refs.first { $0.id == selectedRef }
    }

    private var isJujutsu: Bool {
        refs.contains(where: \.isJujutsu)
    }

    /// Backend-aware isolated-checkout label.
    private var checkoutLabel: String {
        switch checkoutKind {
        case .newWorktree: return isJujutsu ? "New workspace" : "New worktree"
        case .local:
            if selectedRefRow?.worktreePath != nil {
                return isJujutsu ? "Current workspace" : "Current worktree"
            }
            return "Current checkout"
        }
    }

    private var checkoutIcon: LineIcon {
        checkoutKind == .local && selectedRefRow?.worktreePath == nil ? .folder : .folderWithFiles
    }

    /// "From <ref>" only when a new isolated checkout will be created.
    private var refLabel: String {
        guard let row = selectedRefRow else { return "Select ref" }
        return checkoutKind == .newWorktree ? "From \(row.name)" : row.name
    }

    /// An already materialized ref reuses its checkout; otherwise local mode
    /// switches the space checkout through the host's active backend.
    private func pickRef(_ row: RepoRef) async -> String? {
        if row.worktreePath != nil {
            selectedRef = row.id
            checkoutKind = .local
            return nil
        }
        if checkoutKind == .newWorktree || row.current {
            selectedRef = row.id
            return nil
        }
        guard let space else { return nil }
        let requestedSpace = space.id
        let error = await model.switchSpaceRef(space: space, ref: row)
        guard selectedSpaceId == requestedSpace else { return nil }
        if error == nil {
            selectedRef = row.id
            if let reloaded = await model.listRefs(space: space),
               selectedSpaceId == requestedSpace {
                refs = reloaded
            }
        }
        return error
    }

    /// pick_checkout: dropping back to Local with a plain non-current ref
    /// picked drops the pick — the current branch takes over.
    private func pickCheckout(_ kind: CheckoutKind) {
        if kind == .local, checkoutKind == .newWorktree,
           let row = selectedRefRow, row.worktreePath == nil, !row.current {
            selectedRef = refs.first(where: \.current)?.id
        }
        checkoutKind = kind
    }

    private var mentionSearchPath: String? {
        checkoutKind == .local ? selectedRefRow?.worktreePath : nil
    }

    private var mentionContextKey: String {
        "\(selectedSpaceId)|\(mentionSearchPath ?? "")"
    }

    private func refreshMentions() {
        guard let space else {
            mentions.dismiss()
            return
        }
        mentions.update(text: draft, selection: selection, contextKey: mentionContextKey) { query in
            try await model.searchFiles(space: space, path: mentionSearchPath, query: query)
        }
    }

    private func acceptMention(_ match: FileSearchMatch) {
        guard let insertion = mentions.accept(match, in: draft) else { return }
        draft = insertion.text
        selection = insertion.selection
    }

    private var canSend: Bool {
        guard !busy, space != nil, selectedModel != nil else { return false }
        return !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !attachments.isEmpty
    }

    private func offlineNotice(space: Space) -> some View {
        Text("\(model.deviceName(space.deviceId)) is offline — the run will start when it reconnects.")
            .font(Theme.sans(12))
            .foregroundStyle(Theme.warning.opacity(0.9))
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .background(Theme.warning.opacity(0.1), in: RoundedRectangle(cornerRadius: 12))
            .padding(.horizontal, 12)
            .padding(.bottom, 8)
    }

    /// Mint the chat per the checkout plan, queue the first command, and swap
    /// to the live session.
    private func send() {
        guard let space, let selectedModel, canSend else { return }
        let prompt = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        let encodedPrompt = mentions.encodedPrompt(draft)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let staged = attachments
        let shell = parseShellCommand(prompt)
        if hasShellPrefix(prompt), shell == nil {
            sendError = "Enter a Bash command after ! or !!."
            return
        }
        if shell != nil, !staged.isEmpty {
            sendError = "Remove attachments before running a shell command."
            return
        }
        if isGoalCommand(prompt) {
            guard staged.isEmpty else {
                sendError = "Remove attachments before creating a goal."
                return
            }
            showGoalSheet = true
            return
        }
        if prompt == "/answer" || prompt == "/bro" {
            sendError = "Start the session before using this command."
            return
        }

        busy = true
        sendError = nil
        let config = ChatConfig(harness: harness, model: selectedModel.id,
                                reasoning: reasoning, modelOptions: [:],
                                sandbox: "workspace-write")
        Task { @MainActor in
            defer { busy = false }
            var createdChatId: String?
            do {
                var cwd: String?
                var branch = selectedRefRow?.name
                switch checkoutKind {
                case .newWorktree:
                    if let base = selectedRefRow {
                        guard let worktree = await model.createWorktree(space: space, base: base) else {
                            sendError = isJujutsu
                                ? "Couldn't create the Jujutsu workspace."
                                : "Couldn't create the Git worktree."
                            return
                        }
                        cwd = worktree.path
                        branch = worktree.branch
                    }
                case .local:
                    cwd = selectedRefRow?.worktreePath
                }
                guard let chatId = model.createChat(space: space, config: config,
                                                    branch: branch, cwd: cwd),
                      let chat = model.chat(id: chatId),
                      let store = model.sessionStore(for: chat) else {
                    sendError = "Couldn't create the session."
                    return
                }
                createdChatId = chatId
                var attachmentPaths: [String] = []
                for attachment in staged {
                    let uploaded = try await model.uploadAttachment(deviceId: space.deviceId,
                                                                    chatId: chatId,
                                                                    name: attachment.name,
                                                                    data: attachment.data)
                    AttachmentImageCache.shared.seed(deviceId: space.deviceId, path: uploaded,
                                                     name: attachment.name, data: attachment.data)
                    attachmentPaths.append(uploaded)
                }
                if let shell {
                    store.sendBash(command: shell.command,
                                   excludeFromContext: shell.excludeFromContext,
                                   chat: chat)
                } else {
                    let content = withAttachments(text: encodedPrompt, paths: attachmentPaths)
                    store.sendRun(prompt: content, chat: chat, attachments: attachmentPaths)
                }
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                mentions.reset()
                draft = ""
                selection = nil
                attachments = []
                // Replace the canvas with the live session (in-place swap, no
                // back-through-canvas).
                if path.last == .newSession(spaceId: spaceId) {
                    path.removeLast()
                }
                path.append(.chat(chatId))
            } catch {
                if let createdChatId {
                    model.deleteChat(createdChatId)
                }
                sendError = "Attachment upload failed — \(error.localizedDescription)"
            }
        }
    }

    private func createGoalSession(objective: String, tokenBudget: UInt64?) {
        guard let space, let selectedModel else { return }
        busy = true
        sendError = nil
        let config = ChatConfig(harness: harness, model: selectedModel.id,
                                reasoning: reasoning, modelOptions: [:],
                                sandbox: "workspace-write")
        Task { @MainActor in
            defer { busy = false }
            var cwd: String?
            var branch = selectedRefRow?.name
            if checkoutKind == .newWorktree, let base = selectedRefRow {
                guard let worktree = await model.createWorktree(space: space, base: base) else {
                    sendError = isJujutsu
                        ? "Couldn't create the Jujutsu workspace."
                        : "Couldn't create the Git worktree."
                    return
                }
                cwd = worktree.path
                branch = worktree.branch
            } else if checkoutKind == .local {
                cwd = selectedRefRow?.worktreePath
            }
            guard let chatId = model.createChat(space: space, config: config,
                                                branch: branch, cwd: cwd),
                  let chat = model.chat(id: chatId),
                  let store = model.sessionStore(for: chat) else {
                sendError = "Couldn't create the goal session."
                return
            }
            store.createGoal(objective: objective, tokenBudget: tokenBudget)
            mentions.reset()
            draft = ""
            selection = nil
            if path.last == .newSession(spaceId: spaceId) {
                path.removeLast()
            }
            path.append(.chat(chatId))
        }
    }

    private func stage(_ items: [PhotosPickerItem]) {
        Task { @MainActor in
            var failed = 0
            for item in items {
                guard let data = try? await item.loadTransferable(type: Data.self),
                      let staged = StagedAttachment.stage(data: data) else {
                    failed += 1
                    continue
                }
                attachments.append(staged)
            }
            pickerItems = []
            if failed > 0 {
                sendError = failed == 1
                    ? "One image couldn't be attached (unsupported or over 24 MB)."
                    : "\(failed) images couldn't be attached (unsupported or over 24 MB)."
            } else {
                sendError = nil
            }
        }
    }
}

private struct NewGoalSheet: View {
    @Environment(\.dismiss) private var dismiss
    let onCreate: (String, UInt64?) -> Void

    @State private var objective = ""
    @State private var budget = ""
    @State private var error: String?

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 16) {
                TextField("Goal objective", text: $objective, axis: .vertical)
                    .lineLimit(3...8)
                    .font(Theme.sans(14))
                    .padding(12)
                    .background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: 10))
                TextField("Token budget (optional)", text: $budget)
                    .keyboardType(.numberPad)
                    .font(Theme.sans(14))
                    .padding(12)
                    .background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: 10))
                if let error {
                    Text(error)
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.danger)
                }
                Spacer()
            }
            .padding(20)
            .background(Theme.bg)
            .navigationTitle("Create goal")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Create", action: create)
                }
            }
        }
        .presentationDetents([.medium, .large])
    }

    private func create() {
        let trimmed = objective.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            error = "Goal objective is required."
            return
        }
        let tokenBudget: UInt64?
        if budget.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            tokenBudget = nil
        } else if let value = UInt64(budget), value > 0 {
            tokenBudget = value
        } else {
            error = "Token budget must be a positive integer."
            return
        }
        onCreate(trimmed, tokenBudget)
        dismiss()
    }
}

// MARK: - Model / effort picker sheet

/// Detent bottom sheet with harness tabs (hidden once a chat exists), a grouped
/// card of models, and an effort ladder in the same select-row style.
/// Mid-session checkout context: the read-only kind label plus the live ref
/// list (the desktop keeps its branch selector interactive mid-session).
struct SessionCheckoutContext {
    var isWorktree: Bool
    var isJujutsu: Bool
    var cwd: String
    var refs: [RepoRef]
    var currentBranch: String?
    /// Returns the host VCS error to surface inline, or nil on success.
    var onPick: (RepoRef) async -> String?
}

struct ModelPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    @Binding var harness: String
    @Binding var modelId: String
    @Binding var reasoning: String?
    /// True when reconfiguring a live chat: the harness can't change mid-chat.
    var lockedHarness = false
    /// Live per-harness catalogs from the device (static fallback when absent).
    var catalogs: [String: [ModelInfo]] = [:]
    var harnesses = HarnessCatalog.harnesses
    /// Present on live VCS chats: checkout label + switchable refs.
    var checkout: SessionCheckoutContext?

    private func models(for harness: String) -> [ModelInfo] {
        catalogs[harness] ?? HarnessCatalog.models(for: harness)
    }

    @State private var switching: String?
    @State private var switchError: String?

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 22) {
                    if !lockedHarness {
                        HStack(spacing: 8) {
                            ForEach(harnesses) { h in
                                harnessTab(h)
                            }
                            Spacer(minLength: 0)
                        }
                    }

                    VStack(alignment: .leading, spacing: 8) {
                        SheetLabel("Model")
                        SheetCard {
                            let models = models(for: harness)
                            if models.isEmpty {
                                Text(harness == "pi"
                                     ? "Pi is unavailable or has no authenticated models on this device."
                                     : "No models are available on this device.")
                                    .font(Theme.sans(13))
                                    .foregroundStyle(Theme.textMuted)
                                    .padding(16)
                            } else {
                                ForEach(Array(models.enumerated()), id: \.element.id) { ix, m in
                                    SheetSelectRow(title: m.label,
                                                   subtitle: m.description,
                                                   selected: m.id == modelId,
                                                   leading: nil) {
                                        select(model: m)
                                    }
                                    if ix < models.count - 1 {
                                        SheetSeparator()
                                    }
                                }
                            }
                        }
                    }

                    if let m = selectedModel, !m.reasoningLevels.isEmpty {
                        VStack(alignment: .leading, spacing: 8) {
                            SheetLabel("Effort")
                            SheetCard {
                                ForEach(Array(m.reasoningLevels.enumerated()), id: \.element) { ix, level in
                                    SheetSelectRow(title: HarnessCatalog.reasoningLabel(level),
                                                   subtitle: Self.effortHint(level),
                                                   selected: reasoning == level,
                                                   leading: nil) {
                                        reasoning = level
                                    }
                                    if ix < m.reasoningLevels.count - 1 {
                                        SheetSeparator()
                                    }
                                }
                            }
                        }
                    }

                    if let checkout {
                        checkoutSection(checkout)
                    }
                }
                .padding(20)
                .padding(.bottom, 12)
            }
            .background(SheetStyle.panel)
            .navigationTitle("Select model")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
        }
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
    }

    private var selectedModel: ModelInfo? {
        models(for: harness).first { $0.id == modelId }
    }

    private func harnessTab(_ h: HarnessInfo) -> some View {
        let selected = harness == h.id
        return Button {
            guard harness != h.id else { return }
            UISelectionFeedbackGenerator().selectionChanged()
            harness = h.id
            let fallback = models(for: h.id).first
            modelId = fallback?.id ?? ""
            reasoning = fallback.flatMap(HarnessCatalog.defaultReasoning)
        } label: {
            HStack(spacing: 7) {
                HarnessBadge(harness: h.id, size: 15, dimmed: !selected)
                Text(h.label)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(selected ? Theme.text : Theme.textMuted)
            }
            .padding(.horizontal, 14)
            .frame(height: 36)
            .background(selected ? whiteAlpha(0.15) : whiteAlpha(0.05), in: Capsule())
        }
        .buttonStyle(.plain)
    }

    private func select(model m: ModelInfo) {
        modelId = m.id
        if let current = reasoning, m.reasoningLevels.contains(current) {
            return
        }
        reasoning = HarnessCatalog.defaultReasoning(for: m)
    }

    /// Checkout kind plus the live host-VCS ref/revision list.
    @ViewBuilder
    private func checkoutSection(_ checkout: SessionCheckoutContext) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            SheetLabel("Checkout")
            SheetCard {
                HStack(spacing: 12) {
                    LineIconView(checkout.isWorktree ? .folderWithFiles : .folder,
                                 size: 16, color: Theme.textMuted)
                        .frame(width: 22)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(checkout.isWorktree
                             ? (checkout.isJujutsu ? "Workspace" : "Worktree")
                             : "Local checkout")
                            .font(Theme.sans(15))
                            .foregroundStyle(Theme.text)
                        Text(checkout.cwd)
                            .font(Theme.mono(11.5))
                            .foregroundStyle(Theme.textMuted)
                            .lineLimit(1)
                            .truncationMode(.head)
                    }
                    Spacer(minLength: 0)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 11)
            }
        }

        VStack(alignment: .leading, spacing: 8) {
            SheetLabel("Ref")
            if checkout.refs.isEmpty {
                Text("Loading refs from the device…")
                    .font(Theme.sans(13))
                    .foregroundStyle(Theme.textFaint)
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 20)
            } else {
                SheetCard {
                    ForEach(Array(checkout.refs.enumerated()), id: \.element.id) { ix, ref in
                        refRow(ref, checkout: checkout)
                        if ix < checkout.refs.count - 1 {
                            SheetSeparator()
                        }
                    }
                }
            }
            if let switchError {
                Text(switchError)
                    .font(Theme.sans(12.5))
                    .foregroundStyle(Theme.danger)
                    .padding(.horizontal, 4)
            }
        }
    }

    private func refRow(_ ref: RepoRef, checkout: SessionCheckoutContext) -> some View {
        let selected = ref.name == checkout.currentBranch
        return Button {
            guard switching == nil, !selected else { return }
            UISelectionFeedbackGenerator().selectionChanged()
            switchError = nil
            switching = ref.id
            Task { @MainActor in
                let result = await checkout.onPick(ref)
                switching = nil
                switchError = result
            }
        } label: {
            HStack(spacing: 12) {
                LineIconView(.gitBranch, size: 15, color: Theme.textMuted)
                    .frame(width: 20)
                VStack(alignment: .leading, spacing: 2) {
                    Text(ref.name)
                        .font(Theme.sans(15))
                        .foregroundStyle(Theme.text)
                    if let subtitle = refSubtitle(ref, checkout: checkout) {
                        Text(subtitle)
                            .font(Theme.sans(12.5))
                            .foregroundStyle(Theme.textMuted)
                    }
                }
                Spacer(minLength: 8)
                if switching == ref.id {
                    ProgressView()
                        .controlSize(.small)
                        .tint(Theme.textMuted)
                } else {
                    Image(systemName: "checkmark")
                        .font(.system(size: 14, weight: .semibold))
                        .foregroundStyle(Theme.text)
                        .opacity(selected ? 1 : 0)
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }

    private func refSubtitle(_ ref: RepoRef, checkout: SessionCheckoutContext) -> String? {
        if ref.worktreePath == checkout.cwd {
            return checkout.isJujutsu ? "This session's workspace" : "This session's worktree"
        }
        if let worktree = ref.worktreePath, worktree != checkout.cwd {
            return checkout.isJujutsu ? "Switches to its workspace" : "Switches to its worktree"
        }
        if ref.current { return "Current checkout" }
        return ref.kind == .bookmark ? "Bookmark" : nil
    }

    /// One-line hints for the ladder (the special modes deserve explanation).
    static func effortHint(_ level: String) -> String? {
        switch level {
        case "low": return "Fastest responses"
        case "medium": return "Balanced speed and depth"
        case "high": return "Thorough reasoning"
        case "xhigh": return "Extended reasoning"
        case "max": return "Maximum reasoning budget"
        case "ultra": return "Highest Codex tier"
        case "ultracode": return "X-High plus the ultracode setting"
        case "ultrathink": return "Deep-thinking prompt mode"
        default: return nil
        }
    }
}

// MARK: - Ref picker sheet

/// Host-VCS ref selector: Git branches or Jujutsu working copies/bookmarks,
/// with existing isolated checkouts identified inline.
struct RefPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    let refs: [RepoRef]
    let selected: String?
    /// Returns an error message to keep the sheet open, or nil to close.
    let onPick: (RepoRef) async -> String?

    @State private var switching: String?
    @State private var error: String?

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 8) {
                    SheetLabel(isJujutsu ? "Revision" : "Ref")
                    if refs.isEmpty {
                        Text("Loading refs from the device…")
                            .font(Theme.sans(13))
                            .foregroundStyle(Theme.textFaint)
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 28)
                    } else {
                        SheetCard {
                            ForEach(Array(refs.enumerated()), id: \.element.id) { ix, ref in
                                row(ref)
                                if ix < refs.count - 1 {
                                    SheetSeparator()
                                }
                            }
                        }
                    }
                    if let error {
                        Text(error)
                            .font(Theme.sans(12.5))
                            .foregroundStyle(Theme.danger)
                            .padding(.horizontal, 4)
                    }
                }
                .padding(20)
            }
            .background(SheetStyle.panel)
            .navigationTitle(isJujutsu ? "Select revision" : "Select ref")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
        }
        .presentationDetents([.medium])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
    }

    private var isJujutsu: Bool { refs.contains(where: \.isJujutsu) }

    private func row(_ ref: RepoRef) -> some View {
        Button {
            guard switching == nil else { return }
            UISelectionFeedbackGenerator().selectionChanged()
            error = nil
            switching = ref.id
            Task { @MainActor in
                let result = await onPick(ref)
                switching = nil
                if let result {
                    error = result
                } else {
                    dismiss()
                }
            }
        } label: {
            HStack(spacing: 12) {
                LineIconView(.gitBranch, size: 15, color: Theme.textMuted)
                    .frame(width: 20)
                VStack(alignment: .leading, spacing: 2) {
                    Text(ref.name)
                        .font(Theme.sans(15))
                        .foregroundStyle(Theme.text)
                    if let subtitle = subtitle(for: ref) {
                        Text(subtitle)
                            .font(Theme.sans(12.5))
                            .foregroundStyle(Theme.textMuted)
                    }
                }
                Spacer(minLength: 8)
                if switching == ref.id {
                    ProgressView()
                        .controlSize(.small)
                        .tint(Theme.textMuted)
                } else {
                    Image(systemName: "checkmark")
                        .font(.system(size: 14, weight: .semibold))
                        .foregroundStyle(Theme.text)
                        .opacity(ref.id == selected ? 1 : 0)
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }

    private func subtitle(for ref: RepoRef) -> String? {
        if ref.current { return "Current checkout" }
        if ref.worktreePath != nil {
            return ref.isJujutsu ? "Checked out in a workspace" : "Checked out in a worktree"
        }
        if ref.kind == .bookmark { return "Bookmark" }
        return nil
    }
}

// MARK: - Checkout picker sheet

/// Where the session runs: the space folder, an existing isolated checkout,
/// or a fresh Git worktree/Jujutsu workspace created on send.
struct CheckoutPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    let kind: CheckoutKind
    let selectedRefHasWorktree: Bool
    let isJujutsu: Bool
    let onPick: (CheckoutKind) -> Void

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 8) {
                    SheetLabel("Checkout")
                    SheetCard {
                        row(.local,
                            icon: selectedRefHasWorktree ? .folderWithFiles : .folder,
                            title: selectedRefHasWorktree
                                ? (isJujutsu ? "Current workspace" : "Current worktree")
                                : "Current checkout",
                            subtitle: selectedRefHasWorktree
                                ? (isJujutsu
                                   ? "Reuse the picked revision's existing workspace"
                                   : "Reuse the picked ref's existing worktree")
                                : "Run in the space's folder as-is")
                        SheetSeparator()
                        row(.newWorktree, icon: .folderWithFiles,
                            title: isJujutsu ? "New workspace" : "New worktree",
                            subtitle: isJujutsu
                                ? "A fresh isolated workspace created from the picked revision"
                                : "A fresh isolated worktree created off the picked base ref")
                    }
                }
                .padding(20)
            }
            .background(SheetStyle.panel)
            .navigationTitle("Checkout")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
        }
        .presentationDetents([.medium])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
    }

    private func row(_ rowKind: CheckoutKind, icon: LineIcon, title: String, subtitle: String) -> some View {
        Button {
            UISelectionFeedbackGenerator().selectionChanged()
            onPick(rowKind)
            dismiss()
        } label: {
            HStack(spacing: 12) {
                LineIconView(icon, size: 16, color: Theme.textMuted)
                    .frame(width: 22)
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(Theme.sans(15))
                        .foregroundStyle(Theme.text)
                    Text(subtitle)
                        .font(Theme.sans(12.5))
                        .foregroundStyle(Theme.textMuted)
                }
                Spacer(minLength: 8)
                Image(systemName: "checkmark")
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(Theme.text)
                    .opacity(rowKind == kind ? 1 : 0)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }
}
