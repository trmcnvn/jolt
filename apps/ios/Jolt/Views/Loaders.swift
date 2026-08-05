// Loaders + status indicators — ports of crates/ui/src/loaders.rs.

import SwiftUI

/// Violet dotted globe whose fixed lattice carries a smooth brightness sweep.
struct ActivityOrb: View {
    var size: CGFloat = 16
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private struct Dot {
        let x: CGFloat
        let y: CGFloat
        let depth: CGFloat
        let radius: CGFloat
        let opacity: Double
    }

    var body: some View {
        TimelineView(.animation(paused: reduceMotion)) { timeline in
            let phase = reduceMotion
                ? 0
                : (timeline.date.timeIntervalSinceReferenceDate / Motion.activityOrbPeriod)
                    .truncatingRemainder(dividingBy: 1)
            Canvas { context, dimensions in
                let minimumRadius: CGFloat = size <= 10 ? 0.45 : 0.4
                for dot in Self.dots(phase: phase, size: size) {
                    let radius = max(size * dot.radius, minimumRadius)
                    let rect = CGRect(
                        x: dimensions.width * dot.x - radius,
                        y: dimensions.height * dot.y - radius,
                        width: radius * 2,
                        height: radius * 2
                    )
                    context.fill(
                        Path(ellipseIn: rect),
                        with: .color(Theme.inlineCodeText.opacity(dot.opacity))
                    )
                }
            }
            .frame(width: size, height: size)
        }
        .accessibilityLabel("Working")
    }

    private static func dots(phase: Double, size: CGFloat) -> [Dot] {
        let latitudeCount: Int
        let longitudeDensity: Int
        if size >= 24 {
            (latitudeCount, longitudeDensity) = (9, 20)
        } else if size <= 10 {
            (latitudeCount, longitudeDensity) = (4, 8)
        } else {
            (latitudeCount, longitudeDensity) = (6, 12)
        }
        var dots: [Dot] = []
        dots.reserveCapacity(latitudeCount * longitudeDensity)
        for latitude in 0..<latitudeCount {
            let lat = -Double.pi / 2
                + Double(latitude) / Double(latitudeCount - 1) * Double.pi
            let longitudeCount = max(1, Int((abs(cos(lat)) * Double(longitudeDensity)).rounded()))
            for longitude in 0..<longitudeCount {
                dots.append(dot(
                    phase: phase,
                    latitude: latitude,
                    latitudeCount: latitudeCount,
                    longitude: longitude,
                    longitudeCount: longitudeCount
                ))
            }
        }
        return dots.sorted { $0.depth < $1.depth }
    }

    private static func dot(
        phase: Double,
        latitude: Int,
        latitudeCount: Int,
        longitude: Int,
        longitudeCount: Int
    ) -> Dot {
        let lat = -Double.pi / 2
            + Double(latitude) / Double(latitudeCount - 1) * Double.pi
        let lon = Double(longitude) / Double(max(1, longitudeCount)) * Double.pi * 2
        let cosLat = cos(lat)
        let x = cosLat * cos(lon)
        let y = sin(lat)
        let z = cosLat * sin(lon)
        let yaw = 0.55
        let tilt = 0.38
        let x1 = x * cos(yaw) + z * sin(yaw)
        let z1 = -x * sin(yaw) + z * cos(yaw)
        let y1 = y * cos(tilt) - z1 * sin(tilt)
        let z2 = y * sin(tilt) + z1 * cos(tilt)
        let depth = (z2 + 1) * 0.5
        let angle = lon + yaw - phase * Double.pi * 2
        let distance = atan2(sin(angle), cos(angle))
        let boost = exp(-(distance * distance) / 0.16) * max(z2, 0)

        return Dot(
            x: 0.5 + CGFloat(x1) * 0.4,
            y: 0.5 - CGFloat(y1) * 0.4,
            depth: CGFloat(depth),
            radius: 0.024 + CGFloat(depth) * 0.025 + CGFloat(boost) * 0.026,
            opacity: min(1, 0.32 + depth * 0.42 + boost * 0.26)
        )
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
/// activity orb. Exactly 6 wide, like the desktop rail (shell.rs
/// `render_chat_row`) — the session row's lower lines indent by rail + gap, so
/// a wider rail here would push them out of line with the row's first line.
struct StatusRail: View {
    let indicator: ChatIndicator

    var body: some View {
        Group {
            if indicator == .working {
                ActivityOrb(size: 16)
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
    /// Color for marks that carry no brand color of their own (codex, cursor).
    /// Claude keeps its orange regardless.
    var neutral: Color = Theme.text

    var body: some View {
        BrandMarkShape(mark: BrandMark.forHarness(harness))
            .fill((BrandMark.brandTint(for: harness) ?? neutral).opacity(dimmed ? 0.6 : 0.9))
            .frame(width: size, height: size)
    }
}
