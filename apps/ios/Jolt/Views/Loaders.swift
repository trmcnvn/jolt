// Loaders + status indicators — ports of crates/ui/src/loaders.rs.

import SwiftUI

/// Dotted connecting web: drifting nodes wire themselves into a constellation.
struct ActivityOrb: View {
    var size: CGFloat = 16
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.colorScheme) private var colorScheme

    private struct Node {
        let x: Double
        let y: Double
        let z: Double
    }

    private struct Dot {
        let x: CGFloat
        let y: CGFloat
        let depth: Double
        let radius: CGFloat
        let ink: Double
        let opacity: Double
    }

    private struct Line {
        let start: CGPoint
        let end: CGPoint
        let width: CGFloat
        let ink: Double
        let opacity: Double
    }

    private struct Frame {
        let lines: [Line]
        let dots: [Dot]
    }

    var body: some View {
        TimelineView(.animation(paused: reduceMotion)) { timeline in
            let time = reduceMotion
                ? 0.6
                : timeline.date.timeIntervalSinceReferenceDate * Motion.activityWebSpeed
            Canvas { context, dimensions in
                let frame = Self.frame(time: time, size: size)
                for line in frame.lines {
                    var path = Path()
                    path.move(to: CGPoint(
                        x: dimensions.width * line.start.x,
                        y: dimensions.height * line.start.y
                    ))
                    path.addLine(to: CGPoint(
                        x: dimensions.width * line.end.x,
                        y: dimensions.height * line.end.y
                    ))
                    let white = colorScheme == .dark ? 1 - line.ink : line.ink
                    context.stroke(
                        path,
                        with: .color(Color(.sRGB, white: white, opacity: line.opacity)),
                        lineWidth: line.width
                    )
                }
                for dot in frame.dots {
                    let radius = max(dot.radius, 0.3)
                    let rect = CGRect(
                        x: dimensions.width * dot.x - radius,
                        y: dimensions.height * dot.y - radius,
                        width: radius * 2,
                        height: radius * 2
                    )
                    let white = colorScheme == .dark ? 1 - dot.ink : dot.ink
                    context.fill(
                        Path(ellipseIn: rect),
                        with: .color(Color(.sRGB, white: white, opacity: dot.opacity))
                    )
                }
            }
            .frame(width: size, height: size)
        }
        .accessibilityLabel("Working")
    }

    private static func frame(time: Double, size: CGFloat) -> Frame {
        let compact = size <= 10
        let nodeCount = compact ? 8 : 12
        let threshold = compact ? 1.05 : 0.9
        let minimumLineWidth = compact ? 0.75 : 0.9
        let nodeRadius = 1.4 * 1.52
        let nodeRadiusDepth = 1.8 * 1.52
        let radiusScale = pow(Double(size) / 300, 0.6)
        let goldenAngle = Double.pi * (3 - sqrt(5))
        var nodes: [Node] = []
        nodes.reserveCapacity(nodeCount)

        for index in 0..<nodeCount {
            let baseY = 1 - 2 * (Double(index) + 0.5) / Double(nodeCount)
            let radial = sqrt(1 - baseY * baseY)
            let angle = Double(index) * goldenAngle
            let base = Node(x: radial * cos(angle), y: baseY, z: radial * sin(angle))
            let x = base.x + 0.6 * (noise(Double(index) * 0.31 + 9, time * 0.24) - 0.5)
            let y = base.y + 0.6 * (noise(Double(index) * 0.53 + 27, time * 0.21) - 0.5)
            let z = base.z + 0.6 * (noise(Double(index) * 0.77 + 55, time * 0.27) - 0.5)
            let length = sqrt(x * x + y * y + z * z)
            nodes.append(Node(x: x / length, y: y / length, z: z / length))
        }

        var lines: [Line] = []
        for first in 0..<nodeCount {
            for second in (first + 1)..<nodeCount {
                let dx = nodes[first].x - nodes[second].x
                let dy = nodes[first].y - nodes[second].y
                let dz = nodes[first].z - nodes[second].z
                let distance = sqrt(dx * dx + dy * dy + dz * dz)
                guard distance < threshold else { continue }
                let start = project(nodes[first], time: time)
                let end = project(nodes[second], time: time)
                let depth = ((start.depth + end.depth) * 0.5 + 1) * 0.5
                lines.append(Line(
                    start: CGPoint(x: start.x, y: start.y),
                    end: CGPoint(x: end.x, y: end.y),
                    width: CGFloat(max(minimumLineWidth, 0.8 * radiusScale)),
                    ink: 0.42,
                    opacity: min(
                        0.6,
                        (1 - distance / threshold) * (0.3 + 0.55 * depth) * 1.8
                    )
                ))
            }
        }

        var dots: [Dot] = []
        dots.reserveCapacity(nodeCount + 1)
        for (index, node) in nodes.enumerated() {
            let projected = project(node, time: time)
            let depth = (projected.depth + 1) * 0.5
            let pulse = 1 + 0.25 * sin(time * 1.4 + Double(index) * 2.7)
            dots.append(Dot(
                x: projected.x,
                y: projected.y,
                depth: projected.depth,
                radius: CGFloat((nodeRadius + nodeRadiusDepth * depth) * pulse * radiusScale),
                ink: 0.55 - 0.45 * depth,
                opacity: 1
            ))
        }

        let segment = floor(time * 0.55)
        let first = Int(floor(hash(segment, 1.7) * Double(nodeCount)))
        let second = Int(floor(hash(segment, 4.2) * Double(nodeCount)))
        if first != second {
            let progress = time * 0.55 - segment
            let x = nodes[first].x + (nodes[second].x - nodes[first].x) * progress
            let y = nodes[first].y + (nodes[second].y - nodes[first].y) * progress
            let z = nodes[first].z + (nodes[second].z - nodes[first].z) * progress
            let length = max(1e-6, sqrt(x * x + y * y + z * z))
            let projected = project(
                Node(x: x / length, y: y / length, z: z / length),
                time: time
            )
            let depth = (projected.depth + 1) * 0.5
            dots.append(Dot(
                x: projected.x,
                y: projected.y,
                depth: projected.depth,
                radius: CGFloat((nodeRadius * 1.5 + nodeRadiusDepth * depth) * radiusScale),
                ink: 0.05,
                opacity: 0.5 + 0.5 * depth
            ))
        }

        return Frame(lines: lines, dots: dots.sorted { $0.depth < $1.depth })
    }

    private static func project(
        _ node: Node,
        time: Double
    ) -> (x: CGFloat, y: CGFloat, depth: Double) {
        let yaw = time * 0.12
        let cameraTilt = 0.32
        let x = node.x * cos(yaw) + node.z * sin(yaw)
        let z = -node.x * sin(yaw) + node.z * cos(yaw)
        let y = node.y * cos(cameraTilt) - z * sin(cameraTilt)
        let depth = node.y * sin(cameraTilt) + z * cos(cameraTilt)
        return (0.5 + CGFloat(x * 0.4), 0.5 - CGFloat(y * 0.4), depth)
    }

    private static func noise(_ x: Double, _ y: Double) -> Double {
        let xi = floor(x)
        let yi = floor(y)
        var fx = x - xi
        var fy = y - yi
        fx = fx * fx * (3 - 2 * fx)
        fy = fy * fy * (3 - 2 * fy)
        let a = hash(xi, yi)
        let b = hash(xi + 1, yi)
        let c = hash(xi, yi + 1)
        let d = hash(xi + 1, yi + 1)
        return a + (b - a) * fx + (c - a) * fy + (a - b - c + d) * fx * fy
    }

    private static func hash(_ a: Double, _ b: Double) -> Double {
        let value = sin(a * 12.9898 + b * 78.233) * 43_758.5453
        return value - floor(value)
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
    /// Color for marks that carry no brand color of their own (Codex).
    /// Claude keeps its orange regardless.
    var neutral: Color = Theme.text

    var body: some View {
        BrandMarkShape(mark: BrandMark.forHarness(harness))
            .fill((BrandMark.brandTint(for: harness) ?? neutral).opacity(dimmed ? 0.6 : 0.9))
            .frame(width: size, height: size)
    }
}
