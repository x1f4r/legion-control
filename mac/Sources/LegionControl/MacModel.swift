import Foundation
import Observation

/// This Mac as a machine of its own.
///
/// The same agent and the same JSON as anywhere else, but it runs here rather than over ssh, and
/// there is nothing to wake, nothing to boot and nothing to put to sleep: the device the app runs on
/// only ever has services on it.
@MainActor
@Observable
final class MacModel {
    private(set) var config: LocalConfig
    private(set) var status: AgentStatus?
    private(set) var failure: MacAgentError?
    private(set) var lastChecked: Date?
    private(set) var isRefreshing = false
    private(set) var activity: String?
    /// The sentence the local section shows under its buttons after an action.
    private(set) var note: String?
    private(set) var noteIsError = false
    private(set) var noteStamp = Date.distantPast

    private var agent: MacAgent
    private var lastRefreshFinished: Date?

    /// Called whenever anything a viewer would draw has changed.
    var onStateChange: (@MainActor () -> Void)?
    /// How a question reaches the screen.
    var ask: (@MainActor (PendingDialog) -> Void)?

    init(config: LocalConfig) {
        self.config = config
        self.agent = MacAgent(scriptPath: config.agentPath)
    }

    var name: String { config.name }

    var isWorking: Bool { activity != nil }

    var isReachable: Bool { status != nil }

    var services: [ServiceStatus] { status?.resolvedServices ?? [] }

    var isBusy: Bool { status?.busy?.isBusy ?? false }

    var busyReason: String? {
        guard let busy = status?.busy, busy.isBusy else { return nil }
        return busy.summary
    }

    var autoUpdate: Bool? { status?.autoUpdate }

    /// Why the update button for one service on this Mac can do nothing, or nil when there really is
    /// something to do. A build already downloaded and waiting for a quit counts, and so does a
    /// newer build that has not been fetched yet; a source that could not be reached is neither, and
    /// must not be dressed up as an available update.
    func updateUnavailableReason(_ service: ServiceStatus) -> String? {
        guard isReachable else { return "This Mac could not be read, so nothing can be applied." }
        return service.updateUnavailableReason
    }

    /// The second line of the sidebar entry for this Mac. Never a version string: they run to thirty
    /// characters and the column is not that wide.
    var sidebarSummary: String {
        if failure != nil { return "not readable" }
        guard !services.isEmpty else { return "checking" }
        if services.contains(where: { $0.hasStagedUpdate }) { return "update waiting" }
        if services.contains(where: { $0.upToDate == false }) { return "update available" }
        if let reason = busyReason { return reason }
        if services.allSatisfy({ $0.upToDate == true }) { return "up to date" }
        return "version not checked"
    }

    // MARK: - Reading

    /// Skips the call when one is already in flight or one landed a moment ago. Viewers, window
    /// reopens and finished actions all ask at once, and none of them need a second process.
    func refreshIfNeeded(minimumAge: TimeInterval = 2) async {
        if let lastRefreshFinished, Date().timeIntervalSince(lastRefreshFinished) < minimumAge { return }
        await refresh()
    }

    /// The same two guards the remote side keeps. isRefreshing stops two readings running at once;
    /// the activity check stops the poll reading across an action of our own, which on this machine
    /// means reading an app bundle while it is being quit and swapped underneath it. An action that
    /// wants the reading it just earned passes duringAction.
    func refresh(duringAction: Bool = false) async {
        guard !isRefreshing, duringAction || activity == nil else { return }
        isRefreshing = true
        defer {
            isRefreshing = false
            lastRefreshFinished = Date()
            onStateChange?()
        }

        do {
            let reply = try await agent.status()
            guard reply.ok != false else {
                status = nil
                failure = .agentFailed(reply.message ?? "the agent reported a failure")
                lastChecked = Date()
                return
            }
            status = reply
            failure = nil
            lastChecked = Date()
        } catch let error as MacAgentError {
            status = nil
            failure = error
            lastChecked = Date()
        } catch {
            status = nil
            failure = .unreadableOutput(error.localizedDescription)
            lastChecked = Date()
        }
    }

    // MARK: - Acting

    /// The local update is disruptive in a way a remote one is not: it can close the app the user is
    /// sitting in front of. It always asks first, even when nothing is running.
    func requestUpdate(_ service: ServiceStatus) {
        let name = service.displayName
        let staged = service.stagedVersion
        ask?(PendingDialog(
            id: "local-update-\(service.id)",
            title: "Update \(name) on \(config.name)?",
            message: (staged.map { "Build \($0) is downloaded and waiting. " } ?? "")
                + "\(name) quits and starts again to apply it, so anything open in it goes with it. "
                + "If something is running, this Mac holds the update back instead and nothing is interrupted.",
            confirmTitle: "Quit and update",
            perform: { [weak self] in self?.update(service) }
        ))
    }

    func update(_ service: ServiceStatus) {
        let name = service.displayName
        run("Updating \(name) on \(config.name)") {
            do {
                let result = try await self.agent.update(service: self.serviceArgument(service))
                let from = result.from ?? "the installed version"
                let to = result.to ?? "the staged build"
                switch result.action {
                case "updated":
                    self.setNote("\(name) went from \(from) to \(to) and was started again.", isError: false)
                case "noop":
                    self.setNote("Nothing was waiting. \(name) is already on the build it downloaded.", isError: false)
                case "deferred":
                    self.setNote(
                        result.message.map { "Held back: \($0)" }
                            ?? "Held back while something is running. Nothing was interrupted.",
                        isError: false
                    )
                case "downloaded", "staged":
                    self.setNote(
                        result.message ?? "The build was downloaded and is waiting for \(name) to quit.",
                        isError: false
                    )
                default:
                    self.setNote(result.message ?? "The update did not go through.", isError: result.ok == false)
                }
            } catch let error as MacAgentError {
                self.setNote([error.message, error.detail].compactMap { $0 }.joined(separator: " "), isError: true)
            } catch {
                self.setNote(error.localizedDescription, isError: true)
            }
            await self.refresh(duringAction: true)
        }
    }

    func setAutoUpdate(_ enabled: Bool) {
        run("Saving the update setting for \(config.name)") {
            do {
                let result = try await self.agent.setAutoUpdate(enabled)
                let value = result.autoUpdate ?? enabled
                self.setNote(
                    value
                        ? "\(self.config.name) will apply a staged build on its own, once nothing is running."
                        : "\(self.config.name) only updates when you ask.",
                    isError: false
                )
            } catch let error as MacAgentError {
                self.setNote([error.message, error.detail].compactMap { $0 }.joined(separator: " "), isError: true)
            } catch {
                self.setNote(error.localizedDescription, isError: true)
            }
            await self.refresh(duringAction: true)
        }
    }

    /// The same rule as over ssh: an agent that reported no services array has never heard of the
    /// flag, and its one service is the default anyway.
    private func serviceArgument(_ service: ServiceStatus) -> String? {
        status?.reportsServices == true ? service.id : nil
    }

    // MARK: - Plumbing

    private func run(_ label: String, _ work: @escaping @MainActor () async -> Void) {
        guard activity == nil else { return }
        activity = label
        setNote(label + ".", isError: false)
        onStateChange?()
        Task { @MainActor in
            await work()
            self.activity = nil
            self.onStateChange?()
        }
    }

    private func setNote(_ text: String, isError: Bool) {
        note = text
        noteIsError = isError
        noteStamp = Date()
    }
}
