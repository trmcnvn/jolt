// Composer — a floating glass shell with a compact↔expanded morph, 36pt
// controls, and focus widening. It carries the desktop's Send→Steer→Stop
// semantics: live run + text = steer (same
// up-arrow), live run + empty = stop.
//
// The compact→expanded flip is deterministic (newline or >26 chars), NOT
// content-size measured — measurement oscillates at the boundary.

import PhotosUI
import SwiftUI

/// Shared glass shell + input + action row. `chips` (leading accessory views)
/// force the expanded layout — the desktop keeps new-session composers
/// expanded because the pickers need the full row.
struct ComposerShell<Chips: View>: View {
    @Binding var draft: String
    @Binding var selection: TextSelection?
    var placeholder = "Message"
    var sendEnabled: Bool
    var showStop: Bool
    var busy = false
    var onSend: () -> Void
    var onStop: () -> Void = {}
    /// Staged image attachments shown inside the pill. Non-empty forces the
    /// expanded layout, like chips.
    var attachments: [StagedAttachment] = []
    /// Present attachment actions; nil hides the attach button.
    var onAttach: (() -> Void)? = nil
    var onPasteImages: (([NSItemProvider]) -> Void)? = nil
    var onRemoveAttachment: (String) -> Void = { _ in }
    @ViewBuilder var chips: Chips

    @FocusState private var focused: Bool

    private var expanded: Bool {
        Chips.self != EmptyView.self || !attachments.isEmpty
            || draft.contains("\n") || draft.count > 26
    }

    // Switching between VStack/HStack via AnyLayout (rather than an if/else
    // that swaps container types) keeps `input`'s view identity stable across
    // the compact↔expanded flip — an if/else here would tear down and rebuild
    // the TextField, dropping keyboard focus mid-type.
    private var shellLayout: AnyLayout {
        expanded
            ? AnyLayout(VStackLayout(alignment: .leading, spacing: 0))
            : AnyLayout(HStackLayout(alignment: .center, spacing: 12))
    }

    var body: some View {
        shellLayout {
            if expanded, !attachments.isEmpty {
                AttachmentStripView(attachments: attachments, remove: onRemoveAttachment)
                    .padding(.horizontal, 16)
                    .padding(.top, 12)
            }
            if !expanded, onAttach != nil {
                attachButton
                    .padding(.leading, 7)
            }
            input
                .padding(.horizontal, expanded ? 20 : 0)
                .padding(.leading, expanded ? 0 : (onAttach == nil ? 20 : 4))
                .padding(.top, expanded ? 15 : 0)
                .padding(.vertical, expanded ? 0 : 15)
            if expanded {
                HStack(spacing: 10) {
                    if onAttach != nil {
                        attachButton
                    }
                    // Chips scroll; the send button stays pinned.
                    ScrollView(.horizontal, showsIndicators: false) {
                        HStack(spacing: 8) {
                            chips
                        }
                    }
                    .scrollClipDisabled(false)
                    actionButton
                }
                .padding(.horizontal, 10)
                .padding(.top, 10)
                .padding(.bottom, 10)
            } else {
                actionButton
                    .padding(.trailing, 7)
            }
        }
        .background(whiteAlpha(0.04), in: RoundedRectangle(cornerRadius: 28))
        .glassEffect(.regular.interactive(), in: RoundedRectangle(cornerRadius: 28))
        .overlay(RoundedRectangle(cornerRadius: 28).strokeBorder(whiteAlpha(0.05), lineWidth: 1))
        // Focus widening pulls margins in slightly while typing.
        .padding(.horizontal, focused ? 10 : 16)
        .motionAnimation(Motion.resize, value: focused)
        .motionAnimation(Motion.collapse, value: expanded)
    }

    private var input: some View {
        ZStack(alignment: .topLeading) {
            if draft.isEmpty {
                Text(placeholder)
                    .font(Theme.sans(16))
                    .foregroundStyle(Theme.textMuted.opacity(0.6))
                    .allowsHitTesting(false)
            }
            ComposerTextInput(
                text: $draft,
                selection: $selection,
                onPasteImages: onPasteImages
            )
            .focused($focused)
        }
    }

    private var attachButton: some View {
        Button {
            onAttach?()
        } label: {
            Image(systemName: "plus")
                .font(.system(size: 15, weight: .medium))
                .foregroundStyle(Theme.textMuted)
                .frame(width: 36, height: 36)
                .background(whiteAlpha(0.06), in: Circle())
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .disabled(busy)
    }

    /// Attachments count as content: an image-only send is a send, never a stop.
    private var hasContent: Bool {
        !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !attachments.isEmpty
    }

    private var actionButton: some View {
        Button {
            if showStop, !hasContent {
                UIImpactFeedbackGenerator(style: .medium).impactOccurred()
                onStop()
            } else {
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                onSend()
            }
        } label: {
            Group {
                if busy {
                    ProgressView()
                        .controlSize(.small)
                        .tint(Theme.bg)
                } else if showStop, !hasContent {
                    RoundedRectangle(cornerRadius: 3.5)
                        .fill(Theme.bg)
                        .frame(width: 12, height: 12)
                } else {
                    Image(systemName: "arrow.up")
                        .font(.system(size: 16, weight: .semibold))
                        .foregroundStyle(buttonActive ? Theme.bg : Theme.textFaint)
                }
            }
            .frame(width: 36, height: 36)
            .background(buttonActive ? AnyShapeStyle(Theme.text) : AnyShapeStyle(whiteAlpha(0.10)),
                        in: Circle())
            .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .disabled(!buttonActive)
        .motionAnimation(Motion.fadeQuick, value: showStop)
    }

    private var buttonActive: Bool {
        if showStop, !hasContent { return true }
        return sendEnabled && hasContent && !busy
    }
}

/// The live-chat composer: config is locked once the chat exists, so no chips —
/// input, the photo attach button, and the morphing action button.
struct ComposerView: View {
    let store: SessionStore
    let chat: Chat
    let runLive: Bool

    @State private var text = ""
    @State private var selection: TextSelection?
    @State private var mentions = FileMentionDraft()
    @State private var attachments: [StagedAttachment] = []
    @State private var pickerItems: [PhotosPickerItem] = []
    @State private var showPicker = false
    @State private var uploading = false
    @State private var commandBusy = false
    @State private var composerError: String?
    @State private var extractedAnswers: ExtractedAnswerFlow?
    @State private var goalExpanded = false
    @State private var showGoalSheet = false

    var body: some View {
        VStack(spacing: 6) {
            if let goal = chat.goal {
                GoalCard(
                    goal: goal,
                    expanded: $goalExpanded,
                    onEdit: { showGoalSheet = true },
                    onPause: { store.pauseGoal(goal) },
                    onResume: { store.resumeGoal(goal) },
                    onClear: { store.clearGoal(goal) }
                )
            }
            if let composerError {
                Text(composerError)
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.danger)
                    .lineLimit(2)
                    .padding(.horizontal, 24)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if let flow = extractedAnswers {
                ExtractedAnswerPanel(
                    flow: flow,
                    answer: extractedAnswerBinding,
                    onBack: extractedBack,
                    onAdvance: extractedAdvance,
                    onCancel: { extractedAnswers = nil }
                )
            } else {
                commandSuggestions
                FileMentionMenu(draft: mentions, select: acceptMention)
                ComposerShell(
                    draft: $text,
                    selection: $selection,
                    sendEnabled: true,
                    showStop: runLive,
                    busy: uploading || commandBusy,
                    onSend: send,
                    onStop: { store.sendInterrupt() },
                    attachments: attachments,
                    onAttach: { showPicker = true },
                    onPasteImages: pasteImages,
                    onRemoveAttachment: { id in attachments.removeAll { $0.id == id } }
                ) {
                    EmptyView()
                }
            }
        }
        .photosPicker(isPresented: $showPicker, selection: $pickerItems,
                      maxSelectionCount: maxComposerAttachments, matching: .images)
        .onChange(of: pickerItems) { _, items in
            guard !items.isEmpty else { return }
            stage(items)
        }
        .onChange(of: text) { refreshMentions() }
        .onChange(of: selection) { refreshMentions() }
        .sheet(isPresented: $showGoalSheet) {
            GoalManagementSheet(store: store, goal: chat.goal)
        }
    }

    /// Load picked photos into staged attachments (HEIC transcodes to JPEG;
    /// unsupported/oversized picks surface as an error line).
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
                composerError = failed == 1
                    ? "One image couldn't be attached (unsupported or over 24 MB)."
                    : "\(failed) images couldn't be attached (unsupported or over 24 MB)."
            } else {
                composerError = nil
            }
        }
    }

    private func pasteImages(_ providers: [NSItemProvider]) {
        let remaining = maxComposerAttachments - attachments.count
        guard remaining > 0 else {
            composerError = "You can attach up to \(maxComposerAttachments) images."
            return
        }
        Task { @MainActor in
            let result = await stagedPastedAttachments(from: providers, limit: remaining)
            attachments.append(contentsOf: result.attachments)
            if result.imageCount == 0 {
                composerError = "The clipboard doesn't contain an image."
            } else if result.skippedCount > 0 {
                composerError = "You can attach up to \(maxComposerAttachments) images."
            } else if result.failedCount > 0 {
                composerError = result.failedCount == 1
                    ? "One image couldn't be attached (unsupported or over 24 MB)."
                    : "\(result.failedCount) images couldn't be attached (unsupported or over 24 MB)."
            } else {
                composerError = nil
            }
        }
    }

    private func send() {
        let prompt = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let encodedPrompt = mentions.encodedPrompt(text)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let staged = attachments
        guard !prompt.isEmpty || !staged.isEmpty else { return }

        if isGoalCommand(prompt) {
            guard staged.isEmpty else {
                composerError = "Remove attachments before using /goal."
                return
            }
            composerError = nil
            clearDraft()
            showGoalSheet = true
            return
        }
        if !runLive, prompt == "/answer" {
            beginAnswerQuestions()
            return
        }
        if !runLive, prompt == "/bro" {
            guard staged.isEmpty else {
                composerError = "Remove attachments before using /bro."
                return
            }
            store.sendHiddenPrompt(prompt: broPrompt, chat: chat)
            composerError = nil
            clearDraft()
            return
        }
        if hasShellPrefix(prompt) {
            guard staged.isEmpty else {
                composerError = "Remove attachments before running a shell command."
                return
            }
            guard let shell = parseShellCommand(prompt) else {
                composerError = "Enter a Bash command after ! or !!."
                return
            }
            store.sendBash(command: shell.command,
                           excludeFromContext: shell.excludeFromContext,
                           chat: chat)
            composerError = nil
            clearDraft()
            return
        }

        if staged.isEmpty {
            deliver(content: encodedPrompt, paths: [])
            composerError = nil
            clearDraft()
            return
        }
        // Upload first, send after: the refs trailer needs the committed
        // paths, and the doc entry must never point at files that don't
        // exist. The shell shows the spinner (`busy`) while chunks stream.
        uploading = true
        composerError = nil
        Task { @MainActor in
            defer { uploading = false }
            do {
                var uploads: [UploadedAttachment] = []
                for att in staged {
                    let upload = try await store.uploadAttachment(name: att.name, data: att.data)
                    // Seed the cache so our own bubble renders from local
                    // bytes instead of a round-trip.
                    AttachmentImageCache.shared.seed(deviceId: chat.deviceId, path: upload.path,
                                                     name: att.name, data: att.data)
                    uploads.append(upload)
                }
                deliver(content: withAttachments(text: encodedPrompt, uploads: uploads),
                        paths: uploads.map(\.path))
                attachments = []
                clearDraft()
            } catch {
                composerError = "Attachment upload failed — \(error.localizedDescription)"
            }
        }
    }

    @ViewBuilder
    private var commandSuggestions: some View {
        let trimmed = text.trimmingCharacters(in: .whitespaces)
        if !runLive, trimmed.hasPrefix("/"), !trimmed.contains(where: { $0.isWhitespace }) {
            let query = String(trimmed.dropFirst()).lowercased()
            let commands = [
                HarnessInfo(id: "answer", label: "Answer questions from the latest response"),
                HarnessInfo(id: "bro", label: "Restate the latest response plainly"),
                HarnessInfo(id: "goal", label: "Open the long-running goal manager"),
            ].filter { query.isEmpty || $0.id.hasPrefix(query) }
            if !commands.isEmpty {
                VStack(spacing: 0) {
                    ForEach(commands) { command in
                        Button {
                            text = "/\(command.id)"
                        } label: {
                            HStack(spacing: 10) {
                                Text("/\(command.id)")
                                    .font(Theme.mono(12))
                                    .foregroundStyle(Theme.text)
                                Text(command.label)
                                    .font(Theme.sans(12))
                                    .foregroundStyle(Theme.textMuted)
                                Spacer(minLength: 0)
                            }
                            .padding(.horizontal, 14)
                            .frame(height: 38)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                    }
                }
                .background(Theme.surfaceRaised.opacity(0.96), in: RoundedRectangle(cornerRadius: 14))
                .overlay(RoundedRectangle(cornerRadius: 14).strokeBorder(whiteAlpha(0.06)))
                .padding(.horizontal, 16)
            }
        }
    }

    private func beginAnswerQuestions() {
        guard attachments.isEmpty else {
            composerError = "Remove attachments before using /answer."
            return
        }
        guard let source = latestCompletedAssistantMessage() else {
            composerError = "There is no completed assistant response to inspect."
            return
        }
        commandBusy = true
        composerError = nil
        clearDraft()
        Task { @MainActor in
            defer { commandBusy = false }
            do {
                let result = try await store.extractQuestions(sourceMessageId: source)
                guard result.sourceMessageId == source else {
                    composerError = "Question extraction became stale."
                    return
                }
                guard !result.questions.isEmpty else {
                    composerError = "No questions requiring an answer were found."
                    return
                }
                extractedAnswers = ExtractedAnswerFlow(questions: result.questions)
            } catch {
                composerError = "Question extraction failed — \(error.localizedDescription)"
            }
        }
    }

    private func latestCompletedAssistantMessage() -> String? {
        store.entries.reversed().first { entry in
            entry.role == .assistant && entry.status == .complete && entry.parts.contains {
                if case .text(_, let text) = $0 { return !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
                return false
            }
        }?.id
    }

    private var extractedAnswerBinding: Binding<String> {
        Binding(
            get: { extractedAnswers?.currentAnswer ?? "" },
            set: { answer in
                guard var flow = extractedAnswers else { return }
                flow.setCurrentAnswer(answer)
                extractedAnswers = flow
            }
        )
    }

    private func extractedBack() {
        guard var flow = extractedAnswers else { return }
        flow.back()
        extractedAnswers = flow
    }

    private func extractedAdvance() {
        guard var flow = extractedAnswers else { return }
        if flow.advance() {
            extractedAnswers = nil
            store.sendRun(prompt: flow.compiledMessage(), chat: chat)
        } else {
            extractedAnswers = flow
        }
    }

    private func refreshMentions() {
        mentions.update(text: text, selection: selection, contextKey: chat.id) { query in
            try await store.searchFiles(query: query)
        }
    }

    private func acceptMention(_ match: FileSearchMatch) {
        guard let insertion = mentions.accept(match, in: text) else { return }
        text = insertion.text
        selection = insertion.selection
    }

    private func deliver(content: String, paths: [String]) {
        if runLive {
            store.sendSteer(prompt: content)
        } else {
            store.sendRun(prompt: content, chat: chat, attachments: paths)
        }
    }

    private func clearDraft() {
        mentions.reset()
        text = ""
        selection = nil
        // The clear above is unconditional, so a prompt left sitting in the
        // composer after a successful send is not this path failing to run —
        // it is the text view writing the pre-send string back. A focused
        // multiline TextField commits pending autocorrect/marked text through
        // the binding AFTER a programmatic change, which restores the prompt.
        // Re-clear once that has drained; a keystroke can't land inside the
        // same main-actor turn, so this can never eat real input.
        Task { @MainActor in text = "" }
    }
}

func isGoalCommand(_ prompt: String) -> Bool {
    prompt.trimmingCharacters(in: .whitespacesAndNewlines) == "/goal"
}

private struct GoalCard: View {
    let goal: Goal
    @Binding var expanded: Bool
    let onEdit: () -> Void
    let onPause: () -> Void
    let onResume: () -> Void
    let onClear: () -> Void

    var body: some View {
        VStack(spacing: 0) {
            Button { expanded.toggle() } label: {
                HStack(spacing: 8) {
                    Circle()
                        .fill(statusColor)
                        .frame(width: 7, height: 7)
                    Text(statusLabel)
                        .font(Theme.sans(10).weight(.semibold))
                        .foregroundStyle(statusColor)
                    Text(goal.objective)
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                    Spacer(minLength: 4)
                    Text(tokenLabel)
                        .font(Theme.sans(10))
                        .foregroundStyle(Theme.textMuted)
                }
                .padding(.horizontal, 12)
                .frame(height: 38)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if expanded {
                VStack(alignment: .leading, spacing: 9) {
                    Text(goal.objective)
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.text)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    if let message = goal.statusMessage {
                        Text(message)
                            .font(Theme.sans(11))
                            .foregroundStyle(Theme.textMuted)
                    }
                    HStack(spacing: 7) {
                        goalButton(goal.status == .complete ? "New goal" : "Edit", action: onEdit)
                        if goal.status == .active {
                            goalButton("Pause", action: onPause)
                        } else if [.paused, .blocked, .usageLimited].contains(goal.status) {
                            goalButton("Resume", action: onResume)
                        }
                        goalButton("Clear", action: onClear)
                    }
                }
                .padding(12)
                .overlay(alignment: .top) { Rectangle().fill(Theme.border).frame(height: 1) }
            }
        }
        .background(Theme.surfaceRaised.opacity(0.72), in: RoundedRectangle(cornerRadius: 14))
        .overlay(RoundedRectangle(cornerRadius: 14).strokeBorder(statusColor.opacity(0.28)))
        .padding(.horizontal, 16)
    }

    private var statusLabel: String {
        switch goal.status {
        case .active: "ACTIVE"
        case .paused: "PAUSED"
        case .blocked: "BLOCKED"
        case .usageLimited: "USAGE LIMITED"
        case .budgetLimited: "BUDGET REACHED"
        case .complete: "COMPLETE"
        }
    }

    private var statusColor: Color {
        switch goal.status {
        case .active: Theme.accent
        case .complete: Theme.statusCompleted
        default: Theme.warning
        }
    }

    private var tokenLabel: String {
        if let budget = goal.tokenBudget {
            return "\(goal.tokensUsed.formatted()) / \(budget.formatted()) tokens"
        }
        return "\(goal.tokensUsed.formatted()) tokens"
    }

    private func goalButton(_ label: String, action: @escaping () -> Void) -> some View {
        Button(label, action: action)
            .font(Theme.sans(11))
            .foregroundStyle(Theme.text)
            .padding(.horizontal, 10)
            .frame(height: 27)
            .background(whiteAlpha(0.07), in: RoundedRectangle(cornerRadius: 7))
            .buttonStyle(.plain)
    }
}

private struct GoalManagementSheet: View {
    @Environment(\.dismiss) private var dismiss

    let store: SessionStore
    let goal: Goal?

    @State private var objective: String
    @State private var budget: String
    @State private var error: String?

    init(store: SessionStore, goal: Goal?) {
        self.store = store
        self.goal = goal
        let editing = goal?.status != .complete ? goal : nil
        _objective = State(initialValue: editing?.objective ?? "")
        _budget = State(initialValue: editing?.tokenBudget.map { String($0) } ?? "")
    }

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
                if let goal = managedGoal {
                    HStack(spacing: 10) {
                        if goal.status == .active {
                            Button("Pause") {
                                store.pauseGoal(goal)
                                dismiss()
                            }
                        } else if [.paused, .blocked, .usageLimited].contains(goal.status) {
                            Button("Resume") {
                                store.resumeGoal(goal)
                                dismiss()
                            }
                        }
                        Button("Clear", role: .destructive) {
                            store.clearGoal(goal)
                            dismiss()
                        }
                    }
                    .buttonStyle(.bordered)
                }
                Spacer()
            }
            .padding(20)
            .background(Theme.bg)
            .navigationTitle(managedGoal == nil ? "Create goal" : "Manage goal")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(managedGoal == nil ? "Create" : "Save", action: save)
                }
            }
        }
        .presentationDetents([.medium, .large])
    }

    private var managedGoal: Goal? {
        goal?.status == .complete ? nil : goal
    }

    private func save() {
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
        if let goal = managedGoal {
            if let tokenBudget, tokenBudget <= goal.tokensUsed {
                error = "Token budget must exceed the tokens already used."
                return
            }
            store.editGoal(goal, objective: trimmed, tokenBudget: tokenBudget)
        } else {
            store.createGoal(objective: trimmed, tokenBudget: tokenBudget)
        }
        dismiss()
    }
}

// MARK: - Extracted `/answer` questions

struct ExtractedAnswerFlow {
    var questions: [ExtractedQuestion]
    var answers: [String]
    var page = 0

    init(questions: [ExtractedQuestion]) {
        self.questions = questions
        self.answers = Array(repeating: "", count: questions.count)
    }

    var current: ExtractedQuestion { questions[page] }
    var currentAnswer: String { answers[page] }

    mutating func setCurrentAnswer(_ answer: String) {
        answers[page] = answer
    }

    mutating func back() {
        page = max(0, page - 1)
    }

    /// Returns true when the final page was submitted.
    mutating func advance() -> Bool {
        guard page + 1 < questions.count else { return true }
        page += 1
        return false
    }

    func compiledMessage() -> String {
        var lines = ["I answered your questions in the following way:"]
        for (question, answer) in zip(questions, answers) {
            lines.append("")
            lines.append("Q: \(question.question)")
            if let context = question.context { lines.append("> \(context)") }
            let trimmed = answer.trimmingCharacters(in: .whitespacesAndNewlines)
            lines.append("A: \(trimmed.isEmpty ? "(no answer)" : trimmed)")
        }
        return lines.joined(separator: "\n")
    }
}

struct ExtractedAnswerPanel: View {
    let flow: ExtractedAnswerFlow
    @Binding var answer: String
    let onBack: () -> Void
    let onAdvance: () -> Void
    let onCancel: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("ANSWER QUESTIONS")
                    .font(Theme.sans(10.5, weight: .medium))
                    .kerning(1)
                    .foregroundStyle(Theme.textMuted.opacity(0.6))
                Spacer()
                Text("\(flow.page + 1)/\(flow.questions.count)")
                    .font(Theme.sans(10))
                    .foregroundStyle(Theme.textMuted)
                Button(action: onCancel) {
                    Image(systemName: "xmark")
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(Theme.textMuted)
                        .frame(width: 24, height: 24)
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Cancel answering questions")
            }
            Text(flow.current.question)
                .font(Theme.sans(15, weight: .medium))
                .foregroundStyle(Theme.text)
            if let context = flow.current.context {
                Text(context)
                    .font(Theme.sans(12.5))
                    .foregroundStyle(Theme.textMuted)
            }
            TextField("Type your answer", text: $answer, axis: .vertical)
                .font(Theme.sans(14))
                .foregroundStyle(Theme.text)
                .lineLimit(1...5)
                .padding(12)
                .background(whiteAlpha(0.04), in: RoundedRectangle(cornerRadius: 12))
            HStack {
                if flow.page > 0 {
                    Button("Back", action: onBack)
                        .font(Theme.sans(13, weight: .medium))
                        .foregroundStyle(Theme.textMuted)
                }
                Spacer()
                Button(flow.page + 1 < flow.questions.count ? "Next" : "Submit", action: onAdvance)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.bg)
                    .padding(.horizontal, 16)
                    .frame(height: 34)
                    .background(Theme.text, in: Capsule())
            }
        }
        .padding(16)
        .glassEffect(.regular, in: RoundedRectangle(cornerRadius: 26))
        .overlay(RoundedRectangle(cornerRadius: 26).strokeBorder(whiteAlpha(0.05)))
        .padding(.horizontal, 12)
    }
}

// MARK: - Agent question panel

struct QuestionPanel: View {
    let requestId: String
    let questions: [UserInputQuestion]
    let respond: (String, [UserInputAnswer]) -> Void

    @State private var page = 0
    @State private var picked: [String: Set<String>] = [:]  // questionId → labels
    @State private var typed: [String: String] = [:]
    @State private var autoAdvanceTask: Task<Void, Never>?

    var body: some View {
        // `questions[min(page, count - 1)]` traps on an empty list (count - 1
        // is -1). A request whose questions fail to decode reaches here empty,
        // so this crashed the app on any session holding one.
        if questions.isEmpty {
            EmptyView()
        } else {
            panel(for: questions[min(max(page, 0), questions.count - 1)])
        }
    }

    private func panel(for question: UserInputQuestion) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(question.header.uppercased())
                    .font(Theme.sans(10.5, weight: .medium))
                    .kerning(1)
                    .foregroundStyle(Theme.textMuted.opacity(0.6))
                Spacer()
                if questions.count > 1 {
                    Text("\(page + 1)/\(questions.count)")
                        .font(Theme.sans(10))
                        .foregroundStyle(Theme.textMuted)
                        .padding(.horizontal, 6)
                        .frame(height: 20)
                        .background(whiteAlpha(0.06), in: RoundedRectangle(cornerRadius: 6))
                }
            }

            Text(question.question)
                .font(Theme.sans(15, weight: .medium))
                .foregroundStyle(Theme.text)
                .fixedSize(horizontal: false, vertical: true)

            if question.multiSelect == true {
                Text("Select one or more options.")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textMuted)
            }

            VStack(spacing: 4) {
                ForEach(Array(question.options.enumerated()), id: \.offset) { ix, option in
                    optionRow(question: question, ix: ix, option: option)
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Rectangle().fill(whiteAlpha(0.06)).frame(height: 1)
                TextField("Or type your own answer", text: Binding(
                    get: { typed[question.id] ?? "" },
                    set: { typed[question.id] = $0 }
                ))
                .font(Theme.sans(13))
                .foregroundStyle(Theme.text)
                .padding(.top, 6)
            }

            HStack {
                if page > 0 {
                    Button("Back") {
                        page -= 1
                    }
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.textMuted)
                }
                Spacer()
                Button(page < questions.count - 1 ? "Next" : "Submit") {
                    advance()
                }
                .font(Theme.sans(13, weight: .medium))
                .foregroundStyle(Theme.bg)
                .padding(.horizontal, 16)
                .frame(height: 34)
                .background(Theme.text, in: Capsule())
                .opacity(canAdvance(question) ? 1 : 0.4)
                .disabled(!canAdvance(question))
            }
        }
        .padding(16)
        .glassEffect(.regular, in: RoundedRectangle(cornerRadius: 26))
        .overlay(RoundedRectangle(cornerRadius: 26).strokeBorder(whiteAlpha(0.05), lineWidth: 1))
        .padding(.horizontal, 12)
        .transition(.opacity)
    }

    private func optionRow(question: UserInputQuestion, ix: Int, option: String) -> some View {
        let isPicked = (typed[question.id] ?? "").isEmpty
            && picked[question.id, default: []].contains(option)
        return Button {
            pick(question: question, option: option)
        } label: {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(option)
                        .font(Theme.sans(13.5, weight: .medium))
                        .foregroundStyle(Theme.text)
                        .multilineTextAlignment(.leading)
                }
                Spacer(minLength: 0)
                if ix < 9 {
                    Text("\(ix + 1)")
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.textMuted)
                        .frame(width: 22, height: 22)
                        .background(whiteAlpha(0.06), in: RoundedRectangle(cornerRadius: 6))
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(isPicked ? whiteAlpha(0.09) : whiteAlpha(0.025),
                        in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12)
                .strokeBorder(isPicked ? whiteAlpha(0.16) : .clear, lineWidth: 1))
        }
        .buttonStyle(.plain)
    }

    private func pick(question: UserInputQuestion, option: String) {
        typed[question.id] = nil
        if question.multiSelect == true {
            var set = picked[question.id, default: []]
            if set.contains(option) { set.remove(option) } else { set.insert(option) }
            picked[question.id] = set
        } else {
            picked[question.id] = [option]
            // Single-select auto-advances after 220ms (AUTO_ADVANCE_MS).
            autoAdvanceTask?.cancel()
            autoAdvanceTask = Task {
                try? await Task.sleep(nanoseconds: 220_000_000)
                guard !Task.isCancelled else { return }
                advance()
            }
        }
    }

    private func canAdvance(_ question: UserInputQuestion) -> Bool {
        !(typed[question.id] ?? "").isEmpty || !picked[question.id, default: []].isEmpty
    }

    private func advance() {
        let question = questions[min(page, questions.count - 1)]
        guard canAdvance(question) else { return }
        if page < questions.count - 1 {
            page += 1
            return
        }
        let answers = questions.map { q -> UserInputAnswer in
            let typedAnswer = (typed[q.id] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if !typedAnswer.isEmpty {
                return UserInputAnswer(questionId: q.id, labels: [typedAnswer])
            }
            return UserInputAnswer(questionId: q.id, labels: Array(picked[q.id, default: []]))
        }
        respond(requestId, answers)
    }
}
