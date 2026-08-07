// Sheet design language — grouped panel cards, hairline-separated rows, and
// centered headers in Jolt's monochrome theme. Every sheet composes these
// pieces so they feel like one product.

import SwiftUI

enum SheetStyle {
    static let cardRadius: CGFloat = 20
    static let cardFill = whiteAlpha(0.045)
    static let rowSeparator = whiteAlpha(0.06)
    static let panel = grey(0x14)
}

/// Grouped card: rows separated by inset hairlines.
struct SheetCard<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        VStack(spacing: 0) {
            content
        }
        .background(SheetStyle.cardFill, in: RoundedRectangle(cornerRadius: SheetStyle.cardRadius))
        .overlay(RoundedRectangle(cornerRadius: SheetStyle.cardRadius)
            .strokeBorder(whiteAlpha(0.06), lineWidth: 1))
    }
}

/// Inset hairline between card rows.
struct SheetSeparator: View {
    var body: some View {
        Rectangle()
            .fill(SheetStyle.rowSeparator)
            .frame(height: 1)
            .padding(.leading, 16)
    }
}

/// Selectable row: title + optional subtitle, with a compact trailing selection mark.
struct SheetSelectRow: View {
    let title: String
    var subtitle: String?
    var selected: Bool
    var leading: AnyView?
    let action: () -> Void

    var body: some View {
        Button {
            UISelectionFeedbackGenerator().selectionChanged()
            action()
        } label: {
            HStack(spacing: 12) {
                if let leading {
                    leading
                }
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(Theme.sans(15))
                        .foregroundStyle(Theme.text)
                    if let subtitle, !subtitle.isEmpty {
                        Text(subtitle)
                            .font(Theme.sans(12.5))
                            .foregroundStyle(Theme.textMuted)
                            .lineLimit(2)
                    }
                }
                Spacer(minLength: 8)
                SheetSelectionIndicator(selected: selected)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle(selected: selected))
    }
}

/// Native trailing checkmark for a selected item in an iOS choice list.
struct SheetSelectionIndicator: View {
    let selected: Bool

    var body: some View {
        Image(systemName: "checkmark")
            .font(.system(size: 14, weight: .semibold))
            .foregroundStyle(.tint)
            .frame(width: 20)
            .opacity(selected ? 1 : 0)
            .accessibilityHidden(true)
    }
}

/// Uppercase tracked section label above a card.
struct SheetLabel: View {
    let text: String

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        Text(text.uppercased())
            .font(Theme.sans(11, weight: .medium))
            .kerning(1)
            .foregroundStyle(Theme.textMuted.opacity(0.6))
            .padding(.horizontal, 4)
    }
}

/// Brief full-width press feedback; persistent selection is shown by the row itself.
struct SheetRowButtonStyle: ButtonStyle {
    var selected = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(configuration.isPressed ? Theme.elementHover : Color.clear,
                        in: RoundedRectangle(cornerRadius: 8))
            .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

/// Primary pill button pinned at a sheet's bottom.
struct SheetPrimaryButton: View {
    let title: String
    var enabled = true
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(Theme.sans(15, weight: .semibold))
                .foregroundStyle(enabled ? Theme.bg : Theme.textFaint)
                .frame(maxWidth: .infinity)
                .frame(height: 50)
                .background(enabled ? AnyShapeStyle(Theme.text) : AnyShapeStyle(whiteAlpha(0.08)),
                            in: Capsule())
        }
        .buttonStyle(.plain)
        .disabled(!enabled)
    }
}

/// Pressed-state wash for tappable rows and chips — the desktop's
/// `element_hover` (white 6%) translated to touch. Fades out on release.
struct PressWashButtonStyle: ButtonStyle {
    var cornerRadius: CGFloat = 8

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(configuration.isPressed ? Theme.elementHover : Color.clear,
                        in: RoundedRectangle(cornerRadius: cornerRadius))
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}

/// Capsule variant for chips: deepens the existing fill while pressed.
struct ChipPressButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .overlay(Capsule().fill(configuration.isPressed ? whiteAlpha(0.06) : .clear))
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}
