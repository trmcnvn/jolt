// On-device workspace-registry persistence under Application Support.

import Foundation

enum DocDisk {
    private static var directory: URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory,
                                            in: .userDomainMask)[0]
            .appendingPathComponent("JoltDocs", isDirectory: true)
        try? FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        return base
    }

    static func registryURL(orgId: String, userId: String) -> URL {
        directory.appendingPathComponent("registry1_\(orgId)_\(userId).json")
    }

    static func wipeAll() {
        try? FileManager.default.removeItem(at: directory)
    }
}

/// Debounced persistence for the workspace-registry blob. Poke on every
/// mutation; `flush` forces the latest state during backgrounding or teardown.
@MainActor
final class RegistrySaver {
    private let url: URL
    private let data: () -> Data?
    private var generation = 0
    private var dirty = false

    init(url: URL, data: @escaping () -> Data?) {
        self.url = url
        self.data = data
    }

    func poke() {
        dirty = true
        generation += 1
        let expected = generation
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            guard let self, self.generation == expected else { return }
            self.flush()
        }
    }

    func flush() {
        guard dirty else { return }
        dirty = false
        guard let data = data() else { return }
        try? data.write(to: url, options: .atomic)
    }
}
