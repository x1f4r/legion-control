import Foundation

/// What a device knows about its agent's copy of the setup.
struct SetupCopyState {
    /// What this device last decided about the copy.
    var decision: SetupSync.Decision = .unknown
    /// The agent's own lineage, once `config meta` has been read. Read at most once per differing
    /// hash, because status deliberately does not carry it.
    var remoteIdentity: ControllerIdentity?
    var remoteLineageForHash: String?
    /// The document last sent, when it went, and what it ended in.
    var lastShare: (hash: String, at: Date, failure: String?)?
    var isSharing = false
}

/// Everything the divergence sheet needs.
struct SetupDivergence: Sendable, Identifiable {
    var machineId: String
    var machineName: String
    var mine: ControllerDocument
    var theirsBytes: Data
    var theirsIdentity: ControllerIdentity
    var baseBytes: Data?
    var differences: [SetupMerge.Difference]
    /// True when the two are different setups rather than two branches of one.
    var isDifferentSetup: Bool

    var id: String { machineId }
}

// MARK: - Sharing the setup

/// The setup half of a device, written once for the machines over ssh and for this Mac's own agent.
///
/// The local agent is a replica like any other. It is not privileged, it is not the source of
/// truth, and it gets the same fast-forward-or-ask treatment as a machine across the world.
extension AgentDevice {
    /// What this device last decided about its agent's copy of the setup.
    var setupDecision: SetupSync.Decision { core.setup.decision }

    /// The agent's own lineage, once `config meta` has been read.
    var remoteSetupIdentity: ControllerIdentity? { core.setup.remoteIdentity }

    /// What the Setup row says.
    var setupSharing: SetupSharing {
        if let lastShare = core.setup.lastShare, let local = controllerConfig?(), lastShare.hash == local.hash,
           let failure = lastShare.failure {
            return .failed(failure)
        }
        return SetupSync.sharing(
            for: core.setup.decision,
            machine: name,
            local: controllerConfig?(),
            remote: core.setup.remoteIdentity ?? status?.controller?.identity
        )
    }

    /// Decide what to do about this device's copy, and do the safe half of it automatically.
    ///
    /// Two moves need no permission, because both follow strict descent and neither can lose
    /// anything: push when the agent is holding one of this document's ancestors, and fetch when
    /// this document is one of the agent's ancestors. Descent is acyclic, so two devices can never
    /// take turns overwriting each other. Everything else stops and asks.
    func reconcileSetup() {
        guard core.activity == nil, !core.setup.isSharing else { return }
        guard let status else { return }
        let local = controllerConfig?()

        let view = SetupSync.MachineView(
            reportsCopy: status.reportsControllerCopy,
            contract: status.contractVersion,
            hash: status.controllerHash,
            identity: remoteIdentity(for: status),
            lineage: lineageIfCurrent(for: status)
        )
        let decision = SetupSync.decide(local: local, machine: view)
        core.setup.decision = decision

        switch decision {
        case .inSync, .unsupported, .unknown, .diverged, .differentSetup:
            return
        case .push:
            guard let local else { return }
            // One attempt per document per device until the reply is read, then a minute's backoff.
            if let lastShare = core.setup.lastShare, lastShare.hash == local.hash,
               Date().timeIntervalSince(lastShare.at) < SetupSync.retryAfter { return }
            publish(local)
        case .fetch:
            fetchSetup()
        case .readMeta:
            readRemoteSetupMeta()
        }
    }

    private func remoteIdentity(for status: AgentStatus) -> ControllerIdentity? {
        if let identity = core.setup.remoteIdentity, core.setup.remoteLineageForHash == status.controllerHash {
            return identity
        }
        return status.controller?.identity
    }

    /// The agent's lineage, but only when it belongs to the hash the agent is reporting now.
    private func lineageIfCurrent(for status: AgentStatus) -> [String]? {
        guard let hash = core.setup.remoteLineageForHash, hash == status.controllerHash else { return nil }
        return core.setup.remoteIdentity?.lineage ?? []
    }

    /// Send this document, because the agent holds one of its ancestors.
    private func publish(_ local: ControllerDocument) {
        core.setup.isSharing = true
        let dialect = dialect
        Task { @MainActor in
            var failure: String?
            do {
                let request = try AgentRequest.configSet(local, replace: false, dialect: dialect)
                let result = try await self.send(request, decoding: AgentConfigResult.self)
                if result.ok == false {
                    // A refusal here is information, not a fault: the agent has told us our document
                    // does not descend from what it holds, which is exactly the case a person has to
                    // look at.
                    self.core.setup.remoteIdentity = result.identity
                    self.core.setup.remoteLineageForHash = result.storedHash
                    failure = result.reason?.sentence(subject: "the setup", message: result.error ?? result.message)
                        ?? result.error ?? "the agent kept the copy it had."
                } else if let stored = result.storedHash, stored != local.hash {
                    failure = "the agent stored something other than the document it was given."
                }
            } catch let error as AgentFailure {
                failure = error.detailText ?? self.describe(error)
            } catch let invalid as AgentRequest.Invalid {
                failure = invalid.message
            } catch {
                failure = error.localizedDescription
            }

            let sentence = failure.map { "The setup could not be shared with \(self.name): \($0)" }
            self.core.setup.lastShare = (hash: local.hash, at: Date(), failure: sentence)
            if let sentence { self.setStatus(sentence, isError: true) }
            self.core.setup.isSharing = false
            self.onStateChange?()
        }
    }

    /// Read the agent's document and adopt it, because this device is behind.
    private func fetchSetup() {
        core.setup.isSharing = true
        Task { @MainActor in
            defer {
                self.core.setup.isSharing = false
                self.onStateChange?()
            }
            guard let fetched = await self.readRemoteDocument() else { return }
            if let problem = self.adoptSetup?(fetched.bytes, "adopted from \(self.setupHolder)") {
                self.setStatus("The setup from \(self.setupHolder) could not be adopted: \(problem)", isError: true)
            } else {
                self.setStatus("Adopted the setup from \(self.setupHolder): revision \(fetched.identity.revisionNumber).")
            }
        }
    }

    /// Read `config` and check that what came back really is what the agent says it holds.
    ///
    /// Every way this can fail is said out loud. The local side used to return nothing on a failed
    /// read without a word, so a setup this Mac could not read looked exactly like one that was
    /// never asked for.
    private func readRemoteDocument() async -> (bytes: Data, identity: ControllerIdentity)? {
        do {
            let reply = try await send(.configRead(), decoding: RemoteControllerDocument.self)
            guard let document = reply.documentBytes else {
                setStatus("No setup document came back from \(setupHolder).", isError: true)
                return nil
            }
            let canonical = try Canonical.bytes(document)
            let hash = Canonical.sha256(canonical)
            // The hash the agent reports has to match the bytes it sent. Anything else means the
            // document was mangled on the way, and adopting it would spread the damage.
            if let reported = reply.hash, reported != hash {
                setStatus("The setup from \(setupHolder) does not match the hash it reports, so it was not adopted.",
                          isError: true)
                return nil
            }
            return (canonical, reply.meta?.identity ?? ControllerIdentity())
        } catch let error as AgentFailure {
            setStatus("The setup on \(setupHolder) could not be read: \(describe(error))",
                      isError: true, detail: error.detailText)
            return nil
        } catch {
            setStatus("The setup on \(setupHolder) could not be read: \(error.localizedDescription)", isError: true)
            return nil
        }
    }

    /// Read the agent's lineage, which `status` deliberately does not carry.
    private func readRemoteSetupMeta() {
        guard let hash = status?.controllerHash, core.setup.remoteLineageForHash != hash else { return }
        core.setup.isSharing = true
        Task { @MainActor in
            defer {
                self.core.setup.isSharing = false
                self.onStateChange?()
            }
            do {
                let result = try await self.send(.configMeta(), decoding: AgentConfigResult.self)
                self.core.setup.remoteIdentity = result.identity
                self.core.setup.remoteLineageForHash = result.storedHash ?? hash
                self.reconcileSetup()
            } catch {
                // Nothing decided. The next poll asks again.
            }
        }
    }

    /// Gather everything the divergence sheet needs, then raise it.
    func openSetupDivergence() {
        guard let local = controllerConfig?() else { return }
        let isDifferent = core.setup.decision == .differentSetup
        guard isDifferent || core.setup.decision == .diverged else { return }

        Task { @MainActor in
            guard let fetched = await self.readRemoteDocument() else {
                self.onStateChange?()
                return
            }
            let base = self.setupBase?(local.identity, fetched.identity)
            let differences = (try? SetupMerge.differences(mine: local.bytes, theirs: fetched.bytes, base: base)) ?? []
            self.onSetupDiverged?(SetupDivergence(
                machineId: self.historyMachineId,
                machineName: self.name,
                mine: local,
                theirsBytes: fetched.bytes,
                theirsIdentity: fetched.identity,
                baseBytes: base,
                differences: differences,
                isDifferentSetup: isDifferent
            ))
        }
    }

    func requestSetupReplacement() {
        guard case .conflict(let reason) = setupSharing else { return }
        ask?(PendingDialog(
            id: "replace-setup-\(historyMachineId)",
            title: "Replace the setup \(name) is holding?",
            message: "\(reason)\n\nReplacing it overwrites the copy the agent has with the document this app is running on.",
            confirmTitle: "Replace it",
            perform: { [weak self] in self?.replaceSetup() }
        ))
    }

    /// Overwrite the agent's copy with this device's, because the user decided to.
    ///
    /// The only place `--replace` is ever sent, and only after a question with both revisions and
    /// their authors in it.
    func replaceSetup() {
        guard let local = controllerConfig?() else { return }
        let dialect = dialect
        perform("Replacing the setup on \(name)", kind: .shareSetup, subject: "setup") { _ in
            do {
                let request = try AgentRequest.configSet(local, replace: true, dialect: dialect)
                let result = try await self.send(request, decoding: AgentConfigResult.self)
                if result.ok == false {
                    let sentence = result.error ?? result.message ?? "the agent refused the document."
                    self.setStatus("The setup was not replaced: \(sentence)", isError: true)
                    return .init(state: .failed, summary: "The setup on \(self.name) was not replaced. \(sentence)")
                }
                self.core.setup.lastShare = (hash: local.hash, at: Date(), failure: nil)
                self.core.setup.remoteIdentity = nil
                self.core.setup.remoteLineageForHash = nil
                self.setStatus("\(self.name) now holds this device's setup.")
                return .init(state: .succeeded,
                             summary: "\(self.name) now holds revision \(local.identity.revisionNumber).",
                             refresh: true)
            } catch let failure as AgentFailure {
                return self.reportFailure(failure, verb: "Replacing the setup")
            } catch let invalid as AgentRequest.Invalid {
                self.setStatus(invalid.message, isError: true)
                return .init(state: .failed, summary: invalid.message)
            } catch {
                return self.reportFailure(
                    AgentFailure(.unreadableOutput, detail: error.localizedDescription, dispatch: .unknown),
                    verb: "Replacing the setup"
                )
            }
        }
    }

    /// Adopt what the agent holds, dropping this device's branch.
    func adoptRemoteSetup() {
        Task { @MainActor in
            guard let fetched = await self.readRemoteDocument() else {
                self.onStateChange?()
                return
            }
            if let problem = self.adoptSetup?(fetched.bytes, "took the setup from \(self.setupHolder)") {
                self.setStatus(problem, isError: true)
            } else {
                self.setStatus("This device now uses the setup from \(self.setupHolder).")
                self.core.setup.remoteIdentity = nil
                self.core.setup.remoteLineageForHash = nil
                self.core.setup.decision = .unknown
            }
            self.onStateChange?()
        }
    }
}
