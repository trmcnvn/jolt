// Sign-in — the OAuth authorization-code flow against WorkOS AuthKit, with
// the secret-bearing exchange delegated to the edge (`POST /auth/exchange`).
// The jolt mark on black, one white button — the old mobile app's Gate.
//
// Endpoints are fixed to production (the old app's rule: mobile always talks
// to prod; a stale override once broke sign-in in the worst ghost way).

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

/// The actual jolt mark — the desktop's 34-cell logo
/// (crates/ui/assets/icons/jolt-logo.svg), cells scaled from its 820×940
/// viewbox and tinted by `color`.
struct JoltMark: View {
    var color: Color = Theme.text

    /// (x, y) of each 100×100 rx16 cell in the 820×940 viewbox.
    static let cells: [(CGFloat, CGFloat)] = [
        (0, 600), (0, 720), (240, 840), (240, 720), (120, 840), (120, 600),
        (240, 600), (0, 480), (0, 360), (480, 840), (480, 720), (120, 360),
        (120, 240), (240, 360), (600, 720), (480, 600), (360, 360), (240, 240),
        (600, 600), (720, 600), (720, 480), (240, 120), (600, 380), (720, 240),
        (720, 0), (480, 240), (480, 0), (120, 480), (240, 480), (360, 840),
        (360, 720), (360, 600), (360, 480), (120, 720),
    ]

    var body: some View {
        JoltMarkShape()
            .fill(color)
            .aspectRatio(820 / 940, contentMode: .fit)
    }
}

struct JoltMarkShape: Shape {
    func path(in rect: CGRect) -> Path {
        var path = Path()
        let scale = min(rect.width / 820, rect.height / 940)
        let dx = rect.minX + (rect.width - 820 * scale) / 2
        let dy = rect.minY + (rect.height - 940 * scale) / 2
        for (x, y) in JoltMark.cells {
            let cell = CGRect(x: dx + x * scale, y: dy + y * scale,
                              width: 100 * scale, height: 100 * scale)
            path.addRoundedRect(in: cell, cornerSize: CGSize(width: 16 * scale, height: 16 * scale))
        }
        return path
    }
}
