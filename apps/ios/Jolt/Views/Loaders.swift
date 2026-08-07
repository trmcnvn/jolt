// Loaders + status indicators — ports of crates/ui/src/loaders.rs.

import SwiftUI

/// Terminal-style activity spinner, stepped at the glyphs' actual 10fps cadence.
struct ActivitySpinner: View {
    var size: CGFloat = 16
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private static let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]

    var body: some View {
        Group {
            if reduceMotion {
                glyph(Self.frames[0])
            } else {
                TimelineView(.periodic(from: .now, by: 0.1)) { timeline in
                    let tick = Int(timeline.date.timeIntervalSinceReferenceDate * 10)
                    glyph(Self.frames[tick % Self.frames.count])
                }
            }
        }
        .accessibilityLabel("Working")
    }

    private func glyph(_ value: String) -> some View {
        Text(value)
            .font(Theme.mono(size))
            .foregroundStyle(Theme.inlineCodeText)
            .frame(width: size, height: size)
    }
}

// MARK: - Status dot

extension ChatIndicator {
    /// shell/spaces.rs status_dot_color.
    var dotColor: Color {
        switch self {
        case .working: return Theme.statusWorking.opacity(0.85)     // pink-400
        case .awaitingInput: return Theme.accent.opacity(0.9)       // indigo
        case .errored: return Theme.danger
        case .completed: return Theme.statusCompleted.opacity(0.9)  // emerald-400
        case .idle: return whiteAlpha(0.14)
        }
    }
}

/// The 6pt leading dot (leads so its position is stable); Working swaps in the
/// activity spinner. Exactly 6 wide, like the desktop rail (shell.rs
/// `render_chat_row`) — the session row's lower lines indent by rail + gap, so
/// a wider rail here would push them out of line with the row's first line.
struct StatusRail: View {
    let indicator: ChatIndicator

    var body: some View {
        Group {
            if indicator == .working {
                ActivitySpinner(size: 16)
            } else {
                Circle()
                    .fill(indicator.dotColor)
                    .frame(width: 6, height: 6)
            }
        }
        .frame(width: StatusRail.width, height: 10)
    }

    static let width: CGFloat = 6
}

/// Harness brand mark (pickers.rs harness_brand_icon) — the desktop's actual
/// SVG marks, rendered via BrandMarkShape. Claude keeps its brand orange even
/// on the mono surface; others stay neutral (icons.rs convention).
struct HarnessBadge: View {
    let harness: String
    var size: CGFloat = 14
    var dimmed = false
    /// Color for marks that carry no brand color of their own (Codex).
    /// Claude keeps its orange regardless.
    var neutral: Color = Theme.text

    var body: some View {
        BrandMarkShape(mark: BrandMark.forHarness(harness))
            .fill((BrandMark.brandTint(for: harness) ?? neutral).opacity(dimmed ? 0.6 : 0.9))
            .frame(width: size, height: size)
    }
}
