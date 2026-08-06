// Sign-in — the OAuth authorization-code flow against WorkOS AuthKit, with
// the secret-bearing exchange delegated to the edge (`POST /auth/exchange`).
// The Jolt mark on black with one white sign-in button.
//
// Endpoints are fixed to production so a stale override cannot break sign-in.

import AuthenticationServices
import SwiftUI

/// Production cloud endpoints — mirrors edge/wrangler.jsonc.
enum Endpoints {
    static let edgeURL = URL(string: "https://edge.jolt.trmcnvn.dev")!
    static let workosClientId = "client_01KZ8TAWMB4TZHF5RSTJPGR84J"
    static let workosAPIBase = "https://api.workos.com"
    static let callbackScheme = "jolt"

    static func authorizeURL(state: String) -> URL {
        var components = URLComponents(string: "\(workosAPIBase)/user_management/authorize")!
        components.queryItems = [
            URLQueryItem(name: "response_type", value: "code"),
            URLQueryItem(name: "client_id", value: workosClientId),
            URLQueryItem(name: "redirect_uri", value: "\(callbackScheme)://callback"),
            URLQueryItem(name: "provider", value: "authkit"),
            URLQueryItem(name: "state", value: state),
        ]
        return components.url!
    }
}

struct SignInView: View {
    @Environment(AppModel.self) private var model
    @State private var busy = false
    @State private var error: String?
    @State private var authSession = AuthSessionCoordinator()

    var body: some View {
        ZStack {
            Theme.bg.ignoresSafeArea()

            VStack(spacing: 32) {
                Spacer()

                VStack(spacing: 24) {
                    JoltMark()
                        .frame(width: 72, height: 72)
                    VStack(spacing: 6) {
                        Text("Jolt")
                            .font(Theme.sans(28, weight: .semibold))
                            .kerning(-0.5)
                            .foregroundStyle(Theme.text)
                        Text("Your coding agents, from anywhere")
                            .font(Theme.sans(15))
                            .foregroundStyle(Theme.textMuted)
                    }
                }

                VStack(spacing: 12) {
                    Button {
                        signIn()
                    } label: {
                        Group {
                            if busy {
                                ProgressView()
                                    .tint(Theme.bg)
                            } else {
                                Text("Log in to Jolt")
                                    .font(Theme.sans(15, weight: .semibold))
                                    .foregroundStyle(Theme.bg)
                            }
                        }
                        .frame(maxWidth: .infinity)
                        .frame(height: 50)
                        .background(Theme.text, in: RoundedRectangle(cornerRadius: 16))
                    }
                    .buttonStyle(.plain)
                    .disabled(busy)
                    .opacity(busy ? 0.6 : 1)

                    if let error {
                        Text(error)
                            .font(Theme.sans(13))
                            .foregroundStyle(Theme.danger)
                            .multilineTextAlignment(.center)
                    }
                }

                Spacer()
            }
            .padding(.horizontal, 32)
            .frame(maxWidth: 480)
        }
    }

    /// The AuthKit code flow: system browser session → jolt://callback with
    /// code + state → exchange on the edge.
    private func signIn() {
        busy = true
        error = nil
        let state = UUID().uuidString
        authSession.start(url: Endpoints.authorizeURL(state: state),
                          callbackScheme: Endpoints.callbackScheme) { result in
            Task { @MainActor in
                switch result {
                case .cancelled:
                    busy = false
                case .failure(let message):
                    busy = false
                    error = message
                case .success(let callbackURL):
                    let params = URLComponents(url: callbackURL, resolvingAgainstBaseURL: false)?
                        .queryItems ?? []
                    let code = params.first { $0.name == "code" }?.value
                    let cbState = params.first { $0.name == "state" }?.value
                    guard let code, cbState == state else {
                        busy = false
                        error = "Callback missing code or state mismatch"
                        return
                    }
                    do {
                        try await model.signIn(edgeURL: Endpoints.edgeURL, code: code)
                    } catch {
                        self.error = error.localizedDescription
                    }
                    busy = false
                }
            }
        }
    }
}

// MARK: - Auth session plumbing

/// Wraps ASWebAuthenticationSession with a presentation anchor.
@MainActor
final class AuthSessionCoordinator: NSObject, ASWebAuthenticationPresentationContextProviding {
    enum Outcome {
        case success(URL)
        case cancelled
        case failure(String)
    }

    private var session: ASWebAuthenticationSession?

    func start(url: URL, callbackScheme: String, completion: @escaping (Outcome) -> Void) {
        let session = ASWebAuthenticationSession(url: url,
                                                 callbackURLScheme: callbackScheme) { callbackURL, error in
            if let callbackURL {
                completion(.success(callbackURL))
            } else if let error = error as? ASWebAuthenticationSessionError,
                      error.code == .canceledLogin {
                completion(.cancelled)
            } else {
                completion(.failure(error?.localizedDescription ?? "Sign-in failed"))
            }
        }
        session.presentationContextProvider = self
        session.prefersEphemeralWebBrowserSession = false
        self.session = session
        session.start()
    }

    nonisolated func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        MainActor.assumeIsolated {
            UIApplication.shared.connectedScenes
                .compactMap { ($0 as? UIWindowScene)?.keyWindow }
                .first ?? ASPresentationAnchor()
        }
    }
}

/// Jolt's solid lightning bolt, matching the desktop mark in
/// crates/ui/assets/icons/jolt-logo.svg.
struct JoltMark: View {
    var color: Color = Theme.inlineCodeText

    var body: some View {
        JoltMarkShape()
            .fill(color)
            .overlay {
                JoltMarkShape()
                    .stroke(color, style: StrokeStyle(lineWidth: 2, lineJoin: .round))
            }
            .aspectRatio(1, contentMode: .fit)
    }
}

struct JoltMarkShape: Shape {
    func path(in rect: CGRect) -> Path {
        let scale = min(rect.width, rect.height) / 440
        let dx = rect.minX + (rect.width - 440 * scale) / 2
        let dy = rect.minY + (rect.height - 440 * scale) / 2
        let point: (CGFloat, CGFloat) -> CGPoint = { x, y in
            CGPoint(x: dx + x * scale, y: dy + y * scale)
        }

        var path = Path()
        path.move(to: point(245, 58))
        path.addLine(to: point(335, 58))
        path.addLine(to: point(268, 169))
        path.addLine(to: point(316, 169))
        path.addLine(to: point(130, 391))
        path.addLine(to: point(194, 228))
        path.addLine(to: point(139, 228))
        path.closeSubpath()
        return path
    }
}
