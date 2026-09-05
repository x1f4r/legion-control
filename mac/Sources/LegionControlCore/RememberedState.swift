import Foundation

/// The last thing we knew about one system on one machine. Only the system that is awake can be
/// asked, so the switch for every other one falls back to this, clearly marked with when it was
/// recorded.
struct RememberedSystem: Codable, Sendable, Equatable {
    var autoUpdate: Bool?
    var checkedAt: Date?
}

/// Keyed by machine and system together. Two machines are allowed to call their systems the same
/// thing, and a setting read off one of them says nothing about the other.
enum RememberedStore {
    private static let key = "rememberedSystems"

    static func key(machine: String, system: String) -> String { "\(machine)/\(system)" }

    static func load() -> [String: RememberedSystem] {
        guard let data = AppPreferences.data(forKey: key),
              let raw = try? JSONDecoder().decode([String: RememberedSystem].self, from: data)
        else { return [:] }
        return raw
    }

    static func save(_ systems: [String: RememberedSystem]) {
        guard let data = try? JSONEncoder().encode(systems) else { return }
        AppPreferences.set(data, forKey: key)
    }
}
