import SwiftUI
import UniformTypeIdentifiers

/// `TextField` only accepts textual pasteboard content when bound to a
/// `String`. This UIKit-backed editor keeps the native edit menu while routing
/// image pastes into the composer's attachment staging path.
struct ComposerTextInput: UIViewRepresentable {
    @Binding var text: String
    @Binding var selection: TextSelection?
    let onPasteImages: (([NSItemProvider]) -> Void)?

    func makeCoordinator() -> Coordinator {
        Coordinator(parent: self)
    }

    func makeUIView(context: Context) -> ImagePasteTextView {
        let view = ImagePasteTextView()
        view.delegate = context.coordinator
        view.backgroundColor = .clear
        view.font = Theme.sansUI(16)
        view.textColor = UIColor(Theme.text)
        view.tintColor = UIColor(Theme.text)
        view.textContainerInset = .zero
        view.textContainer.lineFragmentPadding = 0
        view.showsVerticalScrollIndicator = false
        view.keyboardDismissMode = .interactive
        view.onPasteImages = context.coordinator.parent.onPasteImages
        return view
    }

    func updateUIView(_ view: ImagePasteTextView, context: Context) {
        context.coordinator.parent = self
        view.onPasteImages = context.coordinator.parent.onPasteImages
        if view.text != text {
            view.text = text
        }
        context.coordinator.applySelection(to: view)
    }

    func sizeThatFits(_ proposal: ProposedViewSize, uiView: ImagePasteTextView,
                      context: Context) -> CGSize? {
        guard let width = proposal.width else { return nil }
        let lineHeight = uiView.font?.lineHeight ?? 20
        let content = uiView.sizeThatFits(
            CGSize(width: width, height: CGFloat.greatestFiniteMagnitude)
        )
        let height = min(max(content.height, lineHeight), lineHeight * 7)
        return CGSize(width: width, height: ceil(height))
    }

    final class Coordinator: NSObject, UITextViewDelegate {
        var parent: ComposerTextInput

        init(parent: ComposerTextInput) {
            self.parent = parent
        }

        func textViewDidChange(_ textView: UITextView) {
            parent.text = textView.text
        }

        func textViewDidChangeSelection(_ textView: UITextView) {
            let text = textView.text ?? ""
            let selected = textView.selectedRange
            guard selected.location != NSNotFound,
                  selected.location + selected.length <= text.utf16.count else { return }
            let lower = String.Index(utf16Offset: selected.location, in: text)
            let upper = String.Index(utf16Offset: selected.location + selected.length, in: text)
            let next = TextSelection(range: lower..<upper)
            if parent.selection != next {
                parent.selection = next
            }
        }

        func applySelection(to textView: UITextView) {
            guard let selection = parent.selection,
                  case .selection(let range) = selection.indices,
                  let lower = range.lowerBound.samePosition(in: parent.text.utf16),
                  let upper = range.upperBound.samePosition(in: parent.text.utf16) else { return }
            let location = parent.text.utf16.distance(from: parent.text.utf16.startIndex, to: lower)
            let length = parent.text.utf16.distance(from: lower, to: upper)
            let selectedRange = NSRange(location: location, length: length)
            if textView.selectedRange != selectedRange {
                textView.selectedRange = selectedRange
            }
        }
    }
}

final class ImagePasteTextView: UITextView {
    var onPasteImages: (([NSItemProvider]) -> Void)?

    override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        if action == #selector(paste(_:)), onPasteImages != nil,
           UIPasteboard.general.hasImages {
            return true
        }
        return super.canPerformAction(action, withSender: sender)
    }

    override func paste(_ sender: Any?) {
        let providers = UIPasteboard.general.itemProviders.filter {
            $0.hasItemConformingToTypeIdentifier(UTType.image.identifier)
        }
        guard !providers.isEmpty, let onPasteImages else {
            super.paste(sender)
            return
        }
        onPasteImages(providers)
    }
}
