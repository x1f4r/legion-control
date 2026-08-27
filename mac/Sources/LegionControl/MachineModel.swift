import Foundation
import Observation

/// One configured machine: what it is running, what its services are doing, and everything that can
/// be done to it.
///
/// There is one of these per machine in the config, each with its own link state, its own poll and
/// its own remembered settings. Nothing about one machine is ever read off another.
@MainActor
@Observable
final class MachineModel: @MainActor Identifiable {
    enum Link: Sendable, Equatable {
        case unknown
        case online(SystemDescriptor)
        /// ssh got through but the agent is not there. The machine is awake, it just cannot be driven.
        case reachableWithoutAgent(SystemConfig)
        case offline(String)
    }

    // MARK: - Observable state

    private(set) var machine: Machine
    private(set) var link: Link = .unknown
    private(set) var status: AgentStatus?
    private(set) var remembered: [String: RememberedSystem] = RememberedStore.load()
    private(set) var lastChecked: Date?
    private(set) var isRefreshing = false
    /// Non-nil while a user-initiated action runs. Doubles as the "controls are disabled" flag.
    private(set) var activity: String?
    private(set) var statusLine = "Ready."
    private(set) var statusIsError = false
    private(set) var statusDetail: String?
    /// When the status line last changed. The window has one status bar for the whole app, so with
    /// more than one machine it shows whichever of them spoke last.
    private(set) var statusStamp = Date.distantPast
    /// What the last action the user started ended in, or nil while none has run since launch.
    /// Cleared when the next one starts. Unlike statusLine this is never written by a poll, so it
    /// is only ever about something the user did.
    private(set) var lastActionOutcome: ActionOutcome?

    struct ActionOutcome: Equatable, Sendable {
        var text: String
        var isError: Bool
    }

    var id: String { machine.id }
    var name: String { machine.name }

    /// Called whenever something a viewer draws has changed.
    var onStateChange: (@MainActor () -> Void)?
    /// How a question reaches the screen. The app owns the one place a question is shown, because
    /// only one of them can be answered at a time whichever machine raised it.
    var ask: (@MainActor (PendingDialog) -> Void)?
    /// The setup this Mac is running on, for the machines to be given a copy of. Read through the
    /// app rather than held here, so an edit to the file is picked up without rebuilding anything.
    var controllerConfig: (@MainActor () -> ControllerDocument?)?

    private var agent: RemoteAgent
    /// Set right after a reboot is handed off, so a dead connection reads as "restarting", not "gone".
    private var rebootExpectation: (target: SystemConfig, until: Date)?
    /// Set right after a sleep is handed off, for the same reason and with the same honesty problem in
    /// reverse: a machine that was deliberately suspended stops answering, and the poll that finds
    /// nothing must not raise a warning about something the user just asked for. Long enough to cover
    /// coming back to the window some minutes later, short enough that a machine which never woke up
    /// stops being explained away.
    private var sleepExpectation: Date?
    private var lastRefreshFinished: Date?
    /// The document last sent to this machine, when it went, and what it ended in. What it is for is
    /// the machine that keeps reporting a stale hash after a push it accepted: without a memory of
    /// the attempt the same bytes would go over every fifteen seconds forever.
    private var lastShare: (hash: String, at: Date, failure: String?)?
    /// True while a push is in flight. Deliberately not `activity`: this is not something the user
    /// asked for, and it must not take the buttons away while it runs.
    private var isSharing = false

    init(machine: Machine) {
        self.machine = machine
        self.agent = RemoteAgent(machine: machine)
    }

    /// How long a machine that will not take the setup is left alone before it is offered again.
    /// Long enough that a machine which cannot store it is not asked every poll, short enough that
    /// whatever was in the way gets another chance without anyone having to do anything.
    private static let shareRetry: TimeInterval = 600

    /// How long an unreachable machine still reads as "asleep" rather than as a problem. The phone
    /// uses the same ten minutes, and the two apps have to say the same thing about the same machine.
    private static let sleepGrace: TimeInterval = 600

    var isWorking: Bool { activity != nil }

    /// True only while the wake packet is in flight. The menu bar needs this rather than isWorking,
    /// which is true for any action at all and would have the wake item claim to be waking the
    /// machine while it is really installing an update.
    var isWaking: Bool { activity == wakeLabel }

    private var wakeLabel: String { "Waking \(machine.name)" }

    /// The system we can actually command. Everything that talks to the agent keys off this.
    var currentSystem: SystemDescriptor? {
        if case .online(let system) = link { return system }
        return nil
    }

    /// The configured system behind the running one, when the config knows it. Only a configured
    /// system carries the argv every command is run through.
    var commandableSystem: SystemConfig? { currentSystem?.configured }

    /// The system that is awake, agent or no agent. Only the wake button cares about the difference.
    var reachableSystem: SystemDescriptor? {
        switch link {
        case .online(let system): system
        case .reachableWithoutAgent(let system): SystemDescriptor(system)
        case .unknown, .offline: nil
        }
    }

    var isAwake: Bool { reachableSystem != nil }

    var canWake: Bool { machine.wake != nil }

    var rebootInProgress: (target: SystemConfig, until: Date)? {
        guard let rebootExpectation, rebootExpectation.until > Date() else { return nil }
        return rebootExpectation
    }

    var busyReason: String? {
        guard let busy = status?.busy, busy.isBusy else { return nil }
        return busy.summary
    }

    /// The services this machine reports right now, in the order the agent lists them.
    var services: [ServiceStatus] { status?.resolvedServices ?? [] }

    func service(id: String) -> ServiceStatus? { status?.service(id: id) }

    var actions: [AgentActionInfo] { (status?.actions ?? []).filter { !$0.id.isEmpty } }

    /// Where the machine can be booted, drawn from the config and filtered by what the agent offers.
    ///
    /// A system is only worth a button when the config knows how to talk to it afterwards, so the
    /// list is the configured systems and never the agent's own ids. An agent that names no
    /// `bootTargets` key at all predates them, and on such a machine every other configured system
    /// is still a plausible target; an agent that names an empty list is saying there are none.
    var bootTargets: [SystemConfig] {
        let running = currentSystem?.id
        let others = machine.systems.filter { $0.id != running }
        guard let reported = status?.bootTargets else { return others }
        let ids = Set(reported.map(\.id))
        return others.filter { ids.contains($0.id) }
    }

    /// What the Setup row says. Everything it needs is in the last status and the last push, so
    /// nothing here has to be kept in step by hand: a machine that goes away stops claiming to be
    /// holding anything the moment its status does.
    var setupSharing: SetupSharing {
        guard let status else { return .unknown }
        guard status.reportsControllerCopy else { return .unsupported }
        guard let local = controllerConfig?() else { return .unknown }
        if status.controllerHash == local.hash { return .upToDate }
        guard let lastShare, lastShare.hash == local.hash else { return .unknown }
        if let failure = lastShare.failure { return .failed(failure) }
        return .justShared
    }

    func autoUpdateValue(for system: SystemConfig) -> Bool? {
        if currentSystem?.id == system.id, let live = status?.autoUpdate { return live }
        return remembered[RememberedStore.key(machine: machine.id, system: system.id)]?.autoUpdate
    }

    func autoUpdateCheckedAt(for system: SystemConfig) -> Date? {
        remembered[RememberedStore.key(machine: machine.id, system: system.id)]?.checkedAt
    }

    // MARK: - How the state reads at a glance

    /// The symbol the menu bar draws for this machine. Template only, so it inverts with the menu
    /// bar itself.
    var symbolName: String {
        if rebootInProgress != nil, !isAwake { return "arrow.triangle.2.circlepath" }
        switch link {
        case .online(let system): return system.symbolName
        case .reachableWithoutAgent: return "exclamationmark.triangle"
        case .offline: return "moon.zzz"
        case .unknown: return "circle.dotted"
        }
    }

    var stateDescription: String {
        if let reboot = rebootInProgress, !isAwake { return "Restarting into \(reboot.target.name)" }
        switch link {
        case .online(let system): return "Running \(system.name)"
        case .reachableWithoutAgent(let system): return "\(system.name) is awake, the control agent is not installed"
        case .offline: return "Asleep or unreachable"
        case .unknown: return "Not checked yet"
        }
    }

    /// The second line of the machine's row in the sidebar. Two or three words, no punctuation to
    /// speak of: it has to survive being read out of the corner of the eye.
    var sidebarSummary: String {
        if let reboot = rebootInProgress, !isAwake { return "restarting into \(reboot.target.name)" }
        switch link {
        case .unknown: return "checking"
        case .offline: return "asleep or unreachable"
        case .reachableWithoutAgent(let system): return "\(system.name), no agent"
        case .online(let system): return system.name
        }
    }

    /// The second line of a service's row. What it says first is whatever would make you open it,
    /// and never a version string: they are far too long to survive a sidebar column.
    func sidebarSummary(for service: ServiceStatus) -> String {
        guard currentSystem != nil else { return "not readable" }
        if service.installed == nil { return "not installed" }
        if service.hasStagedUpdate { return "update waiting" }
        if service.upToDate == false { return "update available" }
        if let reason = service.busyReason { return reason }
        if service.healthy != true && service.running != nil { return "not answering" }
        if service.upToDate == true { return "up to date" }
        return "version not checked"
    }

    // MARK: - Reading

    /// Skips the call when one is already in flight or one landed a moment ago.
    func refreshIfNeeded(minimumAge: TimeInterval) async {
        if let lastRefreshFinished, Date().timeIntervalSince(lastRefreshFinished) < minimumAge { return }
        await refresh(userInitiated: false)
    }

    func refresh(userInitiated: Bool, duringAction: Bool = false) async {
        guard !isRefreshing, duringAction || activity == nil else { return }
        isRefreshing = true
        defer {
            isRefreshing = false
            lastRefreshFinished = Date()
            onStateChange?()
        }

        if userInitiated {
            setStatus("Checking \(machine.name).")
        }

        do {
            let reply = try await agent.status(preferring: commandableSystem)
            // A reply that decoded cleanly can still be the agent reporting its own failure
            // (unsupported platform, unreadable config). Treating ok:false as a healthy machine
            // would paint a green window over a broken agent.
            guard reply.value.ok != false else {
                handleStatusFailure(.agentFailed(reply.value.message ?? "the agent reported a failure"))
                lastChecked = Date()
                return
            }
            apply(reply.value, fallback: reply.system)
            lastChecked = Date()
            if userInitiated {
                setStatus("Status refreshed.")
            }
            shareSetupIfNeeded()
        } catch let error as AgentError {
            handleStatusFailure(error)
            lastChecked = Date()
        } catch {
            handleStatusFailure(.unreadableOutput(error.localizedDescription))
            lastChecked = Date()
        }
    }

    private func apply(_ newStatus: AgentStatus, fallback: SystemConfig) {
        // What the agent says about itself is authoritative; the command shape that worked is the
        // fallback for an agent that says nothing at all.
        let system = SystemDescriptor.resolve(newStatus, on: machine, fallback: fallback)
        status = newStatus
        link = .online(system)
        // The machine answered, so neither expectation has anything left to explain.
        rebootExpectation = nil
        sleepExpectation = nil

        guard let configured = system.configured else { return }
        let key = RememberedStore.key(machine: machine.id, system: configured.id)
        var snapshot = remembered[key] ?? RememberedSystem()
        snapshot.autoUpdate = newStatus.autoUpdate ?? snapshot.autoUpdate
        snapshot.checkedAt = Date()
        remembered[key] = snapshot
        RememberedStore.save(remembered)
    }

    private func handleStatusFailure(_ error: AgentError) {
        switch error {
        case .unreachable(let detail):
            status = nil
            link = .offline(error.message(machine: machine.name))
            if let reboot = rebootInProgress {
                setStatus("Waiting for \(reboot.target.name) to come back.", detail: detail)
            } else if let sleepExpectation, sleepExpectation > Date() {
                // A machine that was deliberately suspended is unreachable on purpose, so the poll
                // that finds nothing is not a fault and must not be drawn as one. The ssh text under
                // it is dropped for the same reason: there is nothing here to diagnose.
                setStatus(canWake
                    ? "\(machine.name) is asleep. Wake brings it back."
                    : "\(machine.name) is asleep.")
            } else {
                setStatus("\(machine.name) is asleep or unreachable.", isError: true, detail: detail)
            }
        case .agentMissing(let system, let detail):
            status = nil
            link = .reachableWithoutAgent(system)
            setStatus("The control agent is missing on \(system.name). Run the installer there.",
                      isError: true, detail: detail)
        default:
            status = nil
            link = .offline(error.message(machine: machine.name))
            setStatus(error.message(machine: machine.name), isError: true, detail: error.detail)
        }
    }

    // MARK: - Sharing the setup

    /// The Mac is the source of truth for the setup, so a machine holding a different copy is given
    /// this one. Nobody asks for this and nobody is told it happened: it is a consequence of having
    /// edited the file, and the only thing worth a sentence is a failure.
    ///
    /// Runs on its own rather than inside the reading that noticed, so a poll is never held up by a
    /// push, and never while an action is in flight: the machine is doing something the user asked
    /// for and this can wait fifteen seconds.
    private func shareSetupIfNeeded() {
        guard activity == nil, !isSharing else { return }
        // An agent that reports no `controller` key predates the whole idea and would reject the
        // command. Nothing is said about it here; the Setup row says it once, quietly.
        guard let status, status.reportsControllerCopy else { return }
        guard let local = controllerConfig?(), status.controllerHash != local.hash else { return }
        if let lastShare, lastShare.hash == local.hash,
           Date().timeIntervalSince(lastShare.at) < Self.shareRetry { return }

        isSharing = true
        Task { @MainActor in
            var failure: String?
            do {
                let reply = try await self.agent.pushControllerConfig(local.bytes, preferring: self.commandableSystem)
                if reply.value.ok == false {
                    failure = reply.value.error ?? "the machine kept the copy it had."
                } else if let stored = reply.value.hash, stored != local.hash {
                    // It took a document that is not the one we sent, so something went wrong on the
                    // way over rather than at either end. Saying it went fine would be a lie the next
                    // status would contradict anyway.
                    failure = "the machine stored something other than the file on this Mac."
                }
            } catch let error as AgentError {
                failure = error.detail ?? error.message(machine: self.machine.name)
            } catch {
                failure = error.localizedDescription
            }

            // Remembered whatever happened, not only after a success. A push that fails the same way
            // every fifteen seconds is as much of a hammering as one the machine never acknowledges,
            // and it would write the same red sentence over the status line each time.
            let sentence = failure.map { "The setup could not be shared with \(self.machine.name): \($0)" }
            self.lastShare = (hash: local.hash, at: Date(), failure: sentence)
            if let sentence { self.setStatus(sentence, isError: true) }
            self.isSharing = false
            self.onStateChange?()
        }
    }

    // MARK: - Power

    func wake() {
        guard let wake = machine.wake else { return }
        run(wakeLabel) {
            self.setStatus("Sending the wake packet.")

            if let failure = await WakeOnLAN.sendMagicPackets(wake) {
                self.setStatus("The wake packet could not be sent.", isError: true, detail: failure)
                return
            }

            guard let probe = wake.probe else {
                // Nothing to watch, so the packet leaving is the whole of what we know. Saying more
                // than that would be inventing a machine coming back.
                self.setStatus("Wake packet sent.")
                self.sleepExpectation = nil
                await self.refreshQuietly()
                return
            }

            self.setStatus("Wake packet sent. Waiting for \(self.machine.name) to answer on port \(probe.probePort).")
            let awake = await WakeOnLAN.waitForProbe(wake, timeout: 45)
            if awake {
                self.setStatus("\(self.machine.name) is awake.")
                self.rebootExpectation = nil
                // It answered, so it is not asleep any more whatever we were expecting.
                self.sleepExpectation = nil
                await self.refreshQuietly()
            } else {
                self.setStatus("No answer within 45 seconds. \(self.machine.name) may still be starting up.",
                               isError: true,
                               detail: "Probed \(probe.host) port \(probe.probePort).")
            }
        }
    }

    func requestSleep() {
        let reason = busyReason
        ask?(PendingDialog(
            id: "sleep-\(machine.id)",
            title: "Put \(machine.name) to sleep?",
            message: {
                if let reason {
                    return "\(machine.name) is working right now (\(reason)). Sleeping interrupts it and loses the work in progress."
                }
                return canWake
                    ? "\(machine.name) suspends to memory now. Wake on LAN stays armed, so the Wake button brings it back."
                    : "\(machine.name) suspends to memory now."
            }(),
            confirmTitle: reason == nil ? "Sleep now" : "Sleep anyway",
            perform: { [weak self] in self?.sleep(force: reason != nil) }
        ))
    }

    /// Suspend to memory. Gated exactly like a reboot, because work in flight is lost either way.
    func sleep(force: Bool) {
        run("Putting \(machine.name) to sleep") {
            do {
                let reply = try await self.agent.sleep(force: force, preferring: self.commandableSystem)
                let result = reply.value
                switch result.action {
                case "sleeping":
                    self.markAsleep(
                        line: self.canWake
                            ? "\(self.machine.name) is going to sleep. Wake brings it back."
                            : "\(self.machine.name) is going to sleep.",
                        detail: result.message
                    )
                case "deferred":
                    let reason = result.message ?? "\(self.machine.name) is busy."
                    self.setStatus("Sleep was held back: \(reason)")
                    // Same shape as the deferred boot: the agent checked the machine as it is now, so
                    // the override is offered rather than taken.
                    if !force {
                        self.offerForce(
                            id: "force-sleep-\(self.machine.id)",
                            title: "Put \(self.machine.name) to sleep anyway?",
                            message: "\(reason) Sleeping now interrupts that work and what is in progress is lost.",
                            confirmTitle: "Sleep anyway"
                        ) { [weak self] in self?.sleep(force: true) }
                    }
                default:
                    self.setStatus("\(self.machine.name) did not go to sleep.", isError: true, detail: result.message)
                }
            } catch let error as AgentError {
                // Suspending can cut the link before ssh returns, exactly as a reboot does.
                if case .unreachable = error, force || self.busyReason == nil {
                    self.markAsleep(
                        line: self.canWake
                            ? "The connection dropped, which is what falling asleep looks like. Wake brings it back."
                            : "The connection dropped, which is what falling asleep looks like.",
                        detail: error.detail
                    )
                } else {
                    self.report(error)
                }
            } catch {
                self.report(.unreadableOutput(error.localizedDescription))
            }
        }
    }

    /// Paint the machine as gone straight away rather than waiting for a poll to find out. A reboot
    /// expectation from earlier is dropped: nothing is coming back on its own now.
    private func markAsleep(line: String, detail: String?) {
        rebootExpectation = nil
        // Without this the next poll finds the machine unreachable and raises the amber warning
        // about a machine the user has just deliberately put to sleep.
        sleepExpectation = Date().addingTimeInterval(Self.sleepGrace)
        status = nil
        link = .offline("Asleep.")
        setStatus(line, detail: detail)
    }

    func requestBoot(into target: SystemConfig) {
        let reason = busyReason
        ask?(PendingDialog(
            id: "boot-\(machine.id)-\(target.id)",
            title: "Boot into \(target.name)?",
            message: {
                if let reason {
                    return "\(machine.name) is working right now (\(reason)). Rebooting into \(target.name) interrupts it and loses the work in progress."
                }
                return "\(machine.name) will reboot now and come back up in \(target.name). This takes about a minute."
            }(),
            confirmTitle: reason == nil ? "Reboot now" : "Switch anyway",
            perform: { [weak self] in self?.boot(into: target, force: reason != nil) }
        ))
    }

    func boot(into target: SystemConfig, force: Bool) {
        run("Switching to \(target.name)") {
            do {
                let reply = try await self.agent.boot(into: target.id, force: force, preferring: self.commandableSystem)
                let result = reply.value
                switch result.action {
                case "rebooting":
                    self.expectReboot(into: target)
                    self.setStatus("Rebooting into \(target.name). This takes about a minute.", detail: result.message)
                case "armed":
                    self.setStatus("The next boot is set to \(target.name). Nothing has rebooted yet.", detail: result.message)
                case "noop":
                    self.setStatus("\(target.name) is already running.", detail: result.message)
                case "deferred":
                    let reason = result.message ?? "\(self.machine.name) is busy."
                    self.setStatus("The switch was held back: \(reason)")
                    self.offerForce(
                        id: "force-boot-\(self.machine.id)-\(target.id)",
                        title: "Switch to \(target.name) anyway?",
                        message: "\(reason) Switching now interrupts that work and what is in progress is lost.",
                        confirmTitle: "Switch anyway"
                    ) { [weak self] in self?.boot(into: target, force: true) }
                default:
                    self.setStatus("The switch to \(target.name) failed.", isError: true, detail: result.message)
                }
            } catch let error as AgentError {
                // The reboot can cut the link before ssh returns, which is expected once it is armed.
                if case .unreachable = error, force || self.busyReason == nil {
                    self.expectReboot(into: target)
                    self.setStatus("The connection dropped, which is what a reboot looks like. Waiting for \(target.name).",
                                   detail: error.detail)
                } else {
                    self.report(error)
                }
            } catch {
                self.report(.unreadableOutput(error.localizedDescription))
            }
        }
    }

    private func expectReboot(into target: SystemConfig) {
        rebootExpectation = (target, Date().addingTimeInterval(180))
        link = .offline("Restarting into \(target.name).")
        status = nil
    }

    // MARK: - Services

    func requestRestart(_ service: ServiceStatus) {
        let reason = busyReason
        let name = service.displayName
        ask?(PendingDialog(
            id: "restart-\(machine.id)-\(service.id)",
            title: "Restart \(name)?",
            message: {
                if let reason {
                    return "\(name) stops and starts again. \(machine.name) looked busy at the last reading (\(reason)), so the restart is checked against the machine as it is now, and held back if something is still running."
                }
                return "\(name) stops and starts again. Anything in flight is dropped."
            }(),
            confirmTitle: "Restart",
            // Never forced from here, however busy the last reading looked. Force is the agent's own
            // check being skipped, and it is only offered back after the agent has looked at the
            // machine as it is now and said no.
            perform: { [weak self] in self?.restart(service, force: false) }
        ))
    }

    /// Not forced on the first attempt, for the same reason the update is not.
    ///
    /// A restart kills whatever is in flight, and the only thing that knows whether something is in
    /// flight is the agent, checking the machine at the moment the command lands. Nothing polls while
    /// no viewer is up, so the busy flag here can be minutes old. Send the plain restart, and if the
    /// agent says no, ask.
    func restart(_ service: ServiceStatus, force: Bool) {
        let name = service.displayName
        run("Restarting \(name)") {
            var awaitingDecision = false
            do {
                let reply = try await self.agent.restart(
                    service: self.serviceArgument(service),
                    force: force,
                    preferring: self.commandableSystem
                )
                let result = reply.value
                switch result.action {
                case "restarted":
                    self.setStatus("\(name) restarted on \(reply.system.name).", detail: result.message)
                case "deferred":
                    let reason = result.message ?? "\(self.machine.name) is busy."
                    self.setStatus("The restart was held back: \(reason)")
                    if !force {
                        awaitingDecision = true
                        self.offerForce(
                            id: "force-restart-\(self.machine.id)-\(service.id)",
                            title: "Restart \(name) anyway?",
                            message: "\(reason) Restarting now interrupts that work and what is in progress is lost.",
                            confirmTitle: "Restart anyway"
                        ) { [weak self] in self?.restart(service, force: true) }
                    }
                default:
                    self.setStatus("The restart failed.", isError: true, detail: result.message)
                }
            } catch let error as AgentError {
                self.report(error)
            } catch {
                self.report(.unreadableOutput(error.localizedDescription))
            }
            // Nothing changed on the machine when the agent held the restart back, and staying in
            // flight while the question is on screen would have run() swallow the answer.
            if !awaitingDecision { await self.refreshQuietly() }
        }
    }

    /// Deliberately NOT forced, and deliberately not judged from what we last read. While no viewer
    /// is up nothing polls, so busyReason here can be minutes old, and work that started since then
    /// would be invisible to us. Send the plain update and let the agent run its own busy check
    /// against the machine as it is right now. If it comes back deferred we ask, which turns force
    /// into something the user chose rather than something a stale reading let through.
    func requestUpdate(_ service: ServiceStatus) {
        update(service, force: false)
    }

    func update(_ service: ServiceStatus, force: Bool) {
        let name = service.displayName
        run("Updating \(name)") {
            var awaitingDecision = false
            do {
                let reply = try await self.agent.update(
                    service: self.serviceArgument(service),
                    force: force,
                    preferring: self.commandableSystem
                )
                let result = reply.value
                let from = result.from ?? "the installed version"
                let to = result.to ?? "the latest build"
                switch result.action {
                case "updated":
                    self.setStatus("\(name) updated from \(from) to \(to).", detail: result.message)
                case "noop":
                    self.setStatus("\(name) is already on the latest build.", detail: result.message)
                case "deferred":
                    let reason = result.message ?? "\(self.machine.name) is busy."
                    self.setStatus("The update was held back: \(reason)", detail: nil)
                    // The agent checked the machine as it is now and said no. Offer the override
                    // rather than performing it, so interrupting work is always a decision.
                    if !force {
                        awaitingDecision = true
                        self.offerForce(
                            id: "force-update-\(self.machine.id)-\(service.id)",
                            title: "Update \(name) anyway?",
                            message: "\(reason) Updating now stops \(name) first, so that work is lost.",
                            confirmTitle: "Update anyway"
                        ) { [weak self] in self?.update(service, force: true) }
                    }
                case "rolled-back":
                    self.setStatus("The update failed and the previous version was put back.",
                                   isError: true, detail: result.message)
                default:
                    self.setStatus("The update failed.", isError: true, detail: result.message)
                }
            } catch let error as AgentError {
                self.report(error)
            } catch {
                self.report(.unreadableOutput(error.localizedDescription))
            }
            // Nothing was installed when the agent held the update back, so there is nothing to
            // re-read. Skipping it also matters for correctness rather than cost: this action is
            // still in flight until the closure returns, and run() drops any action started while
            // one is running, so a user who answers the question quickly would have "Update anyway"
            // swallowed by a status call they cannot see.
            if !awaitingDecision { await self.refreshQuietly() }
        }
    }

    /// The service to name on the command line, or nil for an agent that has never heard of the flag.
    /// The first version looks after exactly one thing, which is the default, so leaving the flag off
    /// runs the same command.
    private func serviceArgument(_ service: ServiceStatus) -> String? {
        status?.reportsServices == true ? service.id : nil
    }

    func setAutoUpdate(_ enabled: Bool, on system: SystemConfig) {
        guard currentSystem?.id == system.id else { return }
        run("Saving the update setting") {
            do {
                let reply = try await self.agent.setAutoUpdate(enabled, preferring: system)
                let value = reply.value.autoUpdate ?? enabled
                let key = RememberedStore.key(machine: self.machine.id, system: system.id)
                var snapshot = self.remembered[key] ?? RememberedSystem()
                snapshot.autoUpdate = value
                snapshot.checkedAt = Date()
                self.remembered[key] = snapshot
                RememberedStore.save(self.remembered)
                self.setStatus(value
                    ? "\(system.name) will update on its own."
                    : "Automatic updates are off on \(system.name).",
                    detail: reply.value.message)
            } catch let error as AgentError {
                self.report(error)
            } catch {
                self.report(.unreadableOutput(error.localizedDescription))
            }
            await self.refreshQuietly()
        }
    }

    // MARK: - Configured actions

    /// An action with its own `confirm` text asks before it runs; one without simply runs. Whether
    /// it is held back while the machine is busy is the agent's call, not ours, and comes back the
    /// same way every other refusal does.
    func requestAction(_ action: AgentActionInfo) {
        guard let confirm = action.confirm, !confirm.isEmpty else {
            run(action, force: false)
            return
        }
        ask?(PendingDialog(
            id: "run-\(machine.id)-\(action.id)",
            title: "\(action.displayName)?",
            message: confirm,
            confirmTitle: action.displayName,
            perform: { [weak self] in self?.run(action, force: false) }
        ))
    }

    func run(_ action: AgentActionInfo, force: Bool) {
        let name = action.displayName
        run(name) {
            var awaitingDecision = false
            do {
                let reply = try await self.agent.run(action: action.id, force: force, preferring: self.commandableSystem)
                let result = reply.value
                switch result.action {
                case "ran":
                    self.setStatus("\(name) ran.", detail: result.message)
                case "deferred":
                    let reason = result.message ?? "\(self.machine.name) is busy."
                    self.setStatus("\(name) was held back: \(reason)")
                    if !force {
                        awaitingDecision = true
                        self.offerForce(
                            id: "force-run-\(self.machine.id)-\(action.id)",
                            title: "Run \(name) anyway?",
                            message: "\(reason) Running it now interrupts that work.",
                            confirmTitle: "Run anyway"
                        ) { [weak self] in self?.run(action, force: true) }
                    }
                default:
                    self.setStatus("\(name) failed.", isError: true, detail: result.message)
                }
            } catch let error as AgentError {
                self.report(error)
            } catch {
                self.report(.unreadableOutput(error.localizedDescription))
            }
            if !awaitingDecision { await self.refreshQuietly() }
        }
    }

    // MARK: - Plumbing

    private func offerForce(
        id: String,
        title: String,
        message: String,
        confirmTitle: String,
        perform: @escaping @MainActor () -> Void
    ) {
        ask?(PendingDialog(id: id, title: title, message: message, confirmTitle: confirmTitle, perform: perform))
    }

    private func run(_ label: String, _ work: @escaping @MainActor () async -> Void) {
        guard activity == nil else { return }
        activity = label
        lastActionOutcome = nil
        setStatus(label + ".")
        onStateChange?()
        Task { @MainActor in
            await work()
            self.activity = nil
            self.lastActionOutcome = ActionOutcome(text: self.statusLine, isError: self.statusIsError)
            self.onStateChange?()
        }
    }

    private func setStatus(_ line: String, isError: Bool = false, detail: String? = nil) {
        statusLine = line
        statusIsError = isError
        statusDetail = detail
        statusStamp = Date()
    }

    /// Re-reads the status after an action without disturbing the sentence the action just wrote, and
    /// without letting go of the controls in between.
    private func refreshQuietly() async {
        let line = statusLine
        let isError = statusIsError
        let detail = statusDetail
        await refresh(userInitiated: false, duringAction: true)
        setStatus(line, isError: isError, detail: detail)
    }

    private func report(_ error: AgentError) {
        setStatus(error.message(machine: machine.name), isError: true, detail: error.detail)
    }
}
