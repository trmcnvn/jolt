// Tabler Icons (Outline) by Paweł Kuna, rendered natively from the upstream
// 24×24 SVG path data. The icons are MIT licensed; see THIRD_PARTY_LICENSES.md.
// Jolt uses a 1.5/24 stroke with Tabler's round caps and joins so the native UI
// matches the desktop client at compact sizes.

import SwiftUI

enum TablerIcon {
    case alertTriangle
    case apps
    case archive
    case arrowDown
    case arrowUp
    case check
    case chevronDown
    case chevronLeft
    case chevronRight
    case clock
    case copy
    case deviceDesktop
    case file
    case filePlus
    case fileText
    case folder
    case folderPlus
    case folders
    case gitBranch
    case listCheck
    case logout
    case messageCircle
    case messages
    case pencil
    case photoExclamation
    case plus
    case search
    case selector
    case square
    case squareCheck
    case terminal
    case trash
    case userCircle
    case users
    case world
    case x

    /// SVG path data from the matching Tabler outline icon.
    var paths: [String] {
        switch self {
        case .alertTriangle:
            return [
                "M12 9v4",
                "M10.363 3.591l-8.106 13.534a1.914 1.914 0 0 0 1.636 2.871h16.214a1.914 1.914 0 0 0 1.636 -2.87l-8.106 -13.536a1.914 1.914 0 0 0 -3.274 0",
                "M12 16h.01",
            ]
        case .apps:
            return [
                "M4 5a1 1 0 0 1 1 -1h4a1 1 0 0 1 1 1v4a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1l0 -4",
                "M4 15a1 1 0 0 1 1 -1h4a1 1 0 0 1 1 1v4a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1l0 -4",
                "M14 15a1 1 0 0 1 1 -1h4a1 1 0 0 1 1 1v4a1 1 0 0 1 -1 1h-4a1 1 0 0 1 -1 -1l0 -4",
                "M14 7l6 0",
                "M17 4l0 6",
            ]
        case .archive:
            return [
                "M3 6a2 2 0 0 1 2 -2h14a2 2 0 0 1 2 2a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2",
                "M5 8v10a2 2 0 0 0 2 2h10a2 2 0 0 0 2 -2v-10",
                "M10 12l4 0",
            ]
        case .arrowDown:
            return ["M12 5l0 14", "M18 13l-6 6", "M6 13l6 6"]
        case .arrowUp:
            return ["M12 5l0 14", "M18 11l-6 -6", "M6 11l6 -6"]
        case .check:
            return ["M5 12l5 5l10 -10"]
        case .chevronDown:
            return ["M6 9l6 6l6 -6"]
        case .chevronLeft:
            return ["M15 6l-6 6l6 6"]
        case .chevronRight:
            return ["M9 6l6 6l-6 6"]
        case .clock:
            return ["M3 12a9 9 0 1 0 18 0a9 9 0 0 0 -18 0", "M12 7v5l3 3"]
        case .copy:
            return [
                "M7 9.667a2.667 2.667 0 0 1 2.667 -2.667h8.666a2.667 2.667 0 0 1 2.667 2.667v8.666a2.667 2.667 0 0 1 -2.667 2.667h-8.666a2.667 2.667 0 0 1 -2.667 -2.667l0 -8.666",
                "M4.012 16.737a2.005 2.005 0 0 1 -1.012 -1.737v-10c0 -1.1 .9 -2 2 -2h10c.75 0 1.158 .385 1.5 1",
            ]
        case .deviceDesktop:
            return [
                "M3 5a1 1 0 0 1 1 -1h16a1 1 0 0 1 1 1v10a1 1 0 0 1 -1 1h-16a1 1 0 0 1 -1 -1v-10",
                "M7 20h10", "M9 16v4", "M15 16v4",
            ]
        case .file:
            return [
                "M14 3v4a1 1 0 0 0 1 1h4",
                "M17 21h-10a2 2 0 0 1 -2 -2v-14a2 2 0 0 1 2 -2h7l5 5v11a2 2 0 0 1 -2 2",
            ]
        case .filePlus:
            return [
                "M14 3v4a1 1 0 0 0 1 1h4",
                "M17 21h-10a2 2 0 0 1 -2 -2v-14a2 2 0 0 1 2 -2h7l5 5v11a2 2 0 0 1 -2 2",
                "M12 11l0 6", "M9 14l6 0",
            ]
        case .fileText:
            return [
                "M14 3v4a1 1 0 0 0 1 1h4",
                "M17 21h-10a2 2 0 0 1 -2 -2v-14a2 2 0 0 1 2 -2h7l5 5v11a2 2 0 0 1 -2 2",
                "M9 9l1 0", "M9 13l6 0", "M9 17l6 0",
            ]
        case .folder:
            return ["M5 4h4l3 3h7a2 2 0 0 1 2 2v8a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-11a2 2 0 0 1 2 -2"]
        case .folderPlus:
            return [
                "M12 19h-7a2 2 0 0 1 -2 -2v-11a2 2 0 0 1 2 -2h4l3 3h7a2 2 0 0 1 2 2v3.5",
                "M16 19h6", "M19 16v6",
            ]
        case .folders:
            return [
                "M9 3h3l2 2h5a2 2 0 0 1 2 2v7a2 2 0 0 1 -2 2h-10a2 2 0 0 1 -2 -2v-9a2 2 0 0 1 2 -2",
                "M17 16v2a2 2 0 0 1 -2 2h-10a2 2 0 0 1 -2 -2v-9a2 2 0 0 1 2 -2h2",
            ]
        case .gitBranch:
            return [
                "M5 18a2 2 0 1 0 4 0a2 2 0 1 0 -4 0",
                "M5 6a2 2 0 1 0 4 0a2 2 0 1 0 -4 0",
                "M15 6a2 2 0 1 0 4 0a2 2 0 1 0 -4 0",
                "M7 8l0 8", "M9 18h6a2 2 0 0 0 2 -2v-5", "M14 14l3 -3l3 3",
            ]
        case .listCheck:
            return [
                "M3.5 5.5l1.5 1.5l2.5 -2.5", "M3.5 11.5l1.5 1.5l2.5 -2.5",
                "M3.5 17.5l1.5 1.5l2.5 -2.5", "M11 6l9 0", "M11 12l9 0", "M11 18l9 0",
            ]
        case .logout:
            return [
                "M14 8v-2a2 2 0 0 0 -2 -2h-7a2 2 0 0 0 -2 2v12a2 2 0 0 0 2 2h7a2 2 0 0 0 2 -2v-2",
                "M9 12h12l-3 -3", "M18 15l3 -3",
            ]
        case .messageCircle:
            return ["M3 20l1.3 -3.9c-2.324 -3.437 -1.426 -7.872 2.1 -10.374c3.526 -2.501 8.59 -2.296 11.845 .48c3.255 2.777 3.695 7.266 1.029 10.501c-2.666 3.235 -7.615 4.215 -11.574 2.293l-4.7 1"]
        case .messages:
            return [
                "M21 14l-3 -3h-7a1 1 0 0 1 -1 -1v-6a1 1 0 0 1 1 -1h9a1 1 0 0 1 1 1v10",
                "M14 15v2a1 1 0 0 1 -1 1h-7l-3 3v-10a1 1 0 0 1 1 -1h2",
            ]
        case .pencil:
            return [
                "M4 20h4l10.5 -10.5a2.828 2.828 0 1 0 -4 -4l-10.5 10.5v4",
                "M13.5 6.5l4 4",
            ]
        case .photoExclamation:
            return [
                "M15 8h.01", "M15 21h-9a3 3 0 0 1 -3 -3v-12a3 3 0 0 1 3 -3h12a3 3 0 0 1 3 3v6",
                "M3 16l5 -5c.928 -.893 2.072 -.893 3 0l4 4",
                "M14 14l1 -1c.665 -.64 1.44 -.821 2.167 -.545", "M19 16v3", "M19 22v.01",
            ]
        case .plus:
            return ["M12 5l0 14", "M5 12l14 0"]
        case .search:
            return ["M3 10a7 7 0 1 0 14 0a7 7 0 1 0 -14 0", "M21 21l-6 -6"]
        case .selector:
            return ["M8 9l4 -4l4 4", "M16 15l-4 4l-4 -4"]
        case .square:
            return ["M3 5a2 2 0 0 1 2 -2h14a2 2 0 0 1 2 2v14a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-14"]
        case .squareCheck:
            return [
                "M3 5a2 2 0 0 1 2 -2h14a2 2 0 0 1 2 2v14a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-14",
                "M9 12l2 2l4 -4",
            ]
        case .terminal:
            return [
                "M8 9l3 3l-3 3", "M13 15l3 0",
                "M3 6a2 2 0 0 1 2 -2h14a2 2 0 0 1 2 2v12a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2l0 -12",
            ]
        case .trash:
            return [
                "M4 7l16 0", "M10 11l0 6", "M14 11l0 6",
                "M5 7l1 12a2 2 0 0 0 2 2h8a2 2 0 0 0 2 -2l1 -12",
                "M9 7v-3a1 1 0 0 1 1 -1h4a1 1 0 0 1 1 1v3",
            ]
        case .userCircle:
            return [
                "M3 12a9 9 0 1 0 18 0a9 9 0 1 0 -18 0",
                "M9 10a3 3 0 1 0 6 0a3 3 0 1 0 -6 0",
                "M6.168 18.849a4 4 0 0 1 3.832 -2.849h4a4 4 0 0 1 3.834 2.855",
            ]
        case .users:
            return [
                "M5 7a4 4 0 1 0 8 0a4 4 0 1 0 -8 0", "M3 21v-2a4 4 0 0 1 4 -4h4a4 4 0 0 1 4 4v2",
                "M16 3.13a4 4 0 0 1 0 7.75", "M21 21v-2a4 4 0 0 0 -3 -3.85",
            ]
        case .world:
            return [
                "M3 12a9 9 0 1 0 18 0a9 9 0 0 0 -18 0", "M3.6 9h16.8", "M3.6 15h16.8",
                "M11.5 3a17 17 0 0 0 0 18", "M12.5 3a17 17 0 0 1 0 18",
            ]
        case .x:
            return ["M18 6l-12 12", "M6 6l12 12"]
        }
    }
}

struct TablerIconShape: Shape {
    let icon: TablerIcon

    func path(in rect: CGRect) -> Path {
        var combined = Path()
        for data in icon.paths {
            combined.addPath(SVGPathParser.path(from: data))
        }
        let scale = min(rect.width, rect.height) / 24
        let dx = rect.minX + (rect.width - 24 * scale) / 2
        let dy = rect.minY + (rect.height - 24 * scale) / 2
        return combined.applying(CGAffineTransform(scaleX: scale, y: scale)
            .concatenating(CGAffineTransform(translationX: dx, y: dy)))
    }
}

/// A native Tabler icon. Omit `color` to inherit the surrounding foreground style.
struct TablerIconView: View {
    let icon: TablerIcon
    var size: CGFloat = 14
    var color: Color?

    init(_ icon: TablerIcon, size: CGFloat = 14, color: Color? = nil) {
        self.icon = icon
        self.size = size
        self.color = color
    }

    private var glyph: some View {
        TablerIconShape(icon: icon)
            .stroke(style: StrokeStyle(lineWidth: 1.5 * size / 24,
                                       lineCap: .round, lineJoin: .round))
            .frame(width: size, height: size)
    }

    @ViewBuilder var body: some View {
        if let color {
            glyph.foregroundStyle(color)
        } else {
            glyph
        }
    }
}

/// A text label whose icon follows the surrounding button/menu foreground style.
struct TablerLabel: View {
    let title: String
    let icon: TablerIcon
    var size: CGFloat = 16

    init(_ title: String, icon: TablerIcon, size: CGFloat = 16) {
        self.title = title
        self.icon = icon
        self.size = size
    }

    var body: some View {
        Label {
            Text(title)
        } icon: {
            TablerIconView(icon, size: size)
        }
    }
}
