import Foundation
import Observation

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

/// Everything remembered about every machine's systems, in one place the whole app shares.
///
/// Each machine model used to load its own copy at launch and write the whole of it back whenever it
/// learned something. The last model to write won: its copy was missing whatever every other model
/// had recorded since launch, so one machine's reading erased another's. There is one store now,
/// owned by the app and handed to every model, and it writes what it holds rather than what one
/// model last saw.
@MainActor
@Observable
final class RememberedSettings {
    private(set) var systems: [String: RememberedSystem]
    @ObservationIgnored private let save: @MainActor ([String: RememberedSystem]) -> Void

    /// The store the app keeps in its preferences.
    convenience init() {
        self.init(systems: RememberedStore.load(), save: { RememberedStore.save($0) })
    }

    init(systems: [String: RememberedSystem], save: @escaping @MainActor ([String: RememberedSystem]) -> Void) {
        self.systems = systems
        self.save = save
    }

    /// A store that forgets everything when it goes, for a model nobody else reads.
    static func inMemory() -> RememberedSettings {
        RememberedSettings(systems: [:], save: { _ in })
    }

    func system(machine: String, system: String) -> RememberedSystem? {
        systems[RememberedStore.key(machine: machine, system: system)]
    }

    /// Note what one system said just now. A reading that did not mention the setting keeps the one
    /// remembered before; the time is always the time of this reading.
    func record(machine: String, system: String, autoUpdate: Bool?) {
        let key = RememberedStore.key(machine: machine, system: system)
        var snapshot = systems[key] ?? RememberedSystem()
        snapshot.autoUpdate = autoUpdate ?? snapshot.autoUpdate
        snapshot.checkedAt = Date()
        systems[key] = snapshot
        save(systems)
    }
}
