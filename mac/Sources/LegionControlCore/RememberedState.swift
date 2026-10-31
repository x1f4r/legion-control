import Foundation
import Observation

/// The last thing we knew about one system on one machine. Only the system that is awake can be
/// asked, so the switch for every other one falls back to this, clearly marked with when it was
/// recorded.
struct RememberedSystem: Codable, Sendable, Equatable {
    var autoUpdate: Bool?
    var checkedAt: Date?
    /// The id of the route the last call to this system went over, and what the machine's routes
    /// were then (MachineModel.routesFingerprint). A launch dials that route first rather than
    /// walking the list from the top, where every dead address costs a whole connect timeout, and
    /// never once the setup reaches the machine some other way.
    var route: String?
    var routes: String?
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

    /// The route that last worked for `system` on `machine`, or with no system named, for whichever
    /// of the machine's systems answered last: at launch nothing says which one is up. Nil when the
    /// machine's routes are not the ones the route was remembered with.
    func route(machine: String, system: String?, routes: String) -> String? {
        let snapshot: RememberedSystem?
        if let system {
            snapshot = self.system(machine: machine, system: system)
        } else {
            snapshot = systems.filter { $0.key.hasPrefix("\(machine)/") && $0.value.route != nil }.values
                .max { ($0.checkedAt ?? .distantPast) < ($1.checkedAt ?? .distantPast) }
        }
        guard let snapshot, snapshot.routes == routes else { return nil }
        return snapshot.route
    }

    /// Note the route a call to one system just went over, with the machine's routes as they were.
    /// Written only when either has changed: every poll that gets through comes here.
    func record(machine: String, system: String, route: String, routes: String) {
        let key = RememberedStore.key(machine: machine, system: system)
        var snapshot = systems[key] ?? RememberedSystem()
        guard snapshot.route != route || snapshot.routes != routes else { return }
        snapshot.route = route
        snapshot.routes = routes
        systems[key] = snapshot
        save(systems)
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
