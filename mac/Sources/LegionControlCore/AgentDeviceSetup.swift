import Foundation

/// What became of a setup a person asked to take from a device's agent.
enum SetupTaking: Sendable, Equatable {
    /// It is applied here.
    case adopted
    /// Nothing is applied yet: what it runs is on screen, waiting for the person to confirm it.
    case awaitingConfirmation
    /// It was not applied, and this says why.
    case refused(String)
}

/// Everything the divergence sheet needs.
///
/// Built only from bytes that hash to what the agent reports and pass section 9.7, after the
/// decision was made again from them: a machine that has moved on to something this device can
/// simply take is taken from instead of shown.
struct SetupDivergence: Sendable, Identifiable {
    var machineId: String
    var machineName: String
    /// The document the sheet was built from. Merge, Keep mine and Take theirs are compare-and-swaps
    /// against its hash: an edit saved while the sheet was open is never overwritten.
    var mine: ControllerDocument
    var theirsBytes: Data
    var theirsHash: String
    var theirsIdentity: ControllerIdentity
    var baseBytes: Data?
    var differences: [SetupMerge.Difference]
    /// True when the two are different setups rather than two branches of one.
    var isDifferentSetup: Bool
    /// Why Merge and Keep mine cannot be used, as the sentence to show, when they cannot: this
    /// device's document has to be fixed in the editor first (section 9.8), or the two could not be
    /// compared at all. Take theirs never waits.
    var mergeBlocker: String?

    var id: String { machineId }
}

// MARK: - The channel

/// How every device's agent is a SetupChannel: through the same `send` everything else goes over.
extension AgentDevice {
    func appliedSetup() -> ControllerDocument? { controllerConfig?() }

    func commitSetup(_ bytes: Data, describedAs description: String, replacing expectedHead: String?) -> ConfigStore.AdoptOutcome {
        adoptSetup?(bytes, description, expectedHead) ?? .refused("The setup has nowhere to go on this device.")
    }

    func configMeta() async throws -> AgentConfigResult {
        try await send(.configMeta(), decoding: AgentConfigResult.self)
    }

    func configRead() async throws -> RemoteControllerDocument {
        try await send(.configRead(), decoding: RemoteControllerDocument.self)
    }

    func configSet(_ document: ControllerDocument, replace: Bool) async throws -> AgentConfigResult {
        try await send(try AgentRequest.configSet(document, replace: replace, dialect: dialect), decoding: AgentConfigResult.self)
    }

    func saySetup(_ line: String, isError: Bool, detail: String?) {
        setStatus(line, isError: isError, detail: detail)
    }

    func setupDidChange() { onStateChange?() }
}

// MARK: - Sharing the setup

extension AgentDevice {
    /// What this device last decided about its agent's copy of the setup.
    var setupDecision: SetupSync.Verdict { core.setup.verdict }

    /// What the agent said about the document it holds now.
    var remoteSetupIdentity: ControllerIdentity? { core.setup.remoteIdentity(for: status) }

    /// What the Setup row says.
    var setupSharing: SetupSharing {
        core.setup.sharing(holder: setupHolder, local: controllerConfig?(), status: status)
    }

    /// Decide what to do about this device's copy, and do the safe half of it automatically.
    func reconcileSetup() {
        core.setup.reconcile(over: self)
    }

    /// Decide again from the status already read, because the setup here changed under a question.
    func compareSetupAgain() {
        core.setup.decideAgain(over: self)
    }

    /// Send a push that was held after a refusal again, because a person asked.
    func tryAgainSharingSetup() {
        core.setup.tryAgain(over: self)
    }

    /// Read the agent's document, decide again from it, and raise the divergence sheet when it still
    /// diverges.
    func openSetupDivergence() {
        guard let local = controllerConfig?() else { return }
        guard [.diverged, .differentSetup].contains(core.setup.verdict.decision) else { return }

        Task { @MainActor in
            defer { self.onStateChange?() }
            guard let theirs = await self.verifiedRemoteDocument() else { return }
            let verdict = self.core.setup.decideAgain(after: theirs, over: self)
            guard [.diverged, .differentSetup].contains(verdict.decision) else { return }
            let base = self.setupBase?(local.hash, local.identity.ancestors, theirs.hash, theirs.identity.ancestors)
            // Two documents that could not be compared are said to be that, never drawn as two that
            // differ in nothing: a kept base that peer intake refuses, for one, stops the merge just
            // as it stops the comparison.
            var blocker: String?
            var differences: [SetupMerge.Difference] = []
            if verdict.decision == .diverged, let problem = SetupMerge.blocker(mine: local.bytes) {
                blocker = "This device's setup has to be fixed in the editor before it can be merged or kept: \(problem)"
            } else {
                do {
                    differences = try SetupMerge.differences(base: base, mine: local.bytes, theirs: theirs.bytes)
                } catch let failure as SetupMerge.MergeFailure {
                    blocker = failure.message
                } catch {
                    blocker = "The two setups could not be compared: \(error.localizedDescription)"
                }
            }
            self.onSetupDiverged?(SetupDivergence(
                machineId: self.historyMachineId,
                machineName: self.name,
                mine: local,
                theirsBytes: theirs.bytes,
                theirsHash: theirs.hash,
                theirsIdentity: theirs.identity,
                baseBytes: base,
                differences: differences,
                isDifferentSetup: verdict.decision == .differentSetup,
                mergeBlocker: blocker
            ))
        }
    }

    /// The agent's document, when it is the agent's own bytes and section 9.7 accepts it. Anything
    /// else is said on the device's line and gives nil: neither a merge nor taking it may start from
    /// bytes nobody can vouch for.
    private func verifiedRemoteDocument() async -> SetupPeerSync.VerifiedRead? {
        switch await core.setup.readDocument(over: self) {
        case .failed(let sentence, let detail):
            setStatus(sentence, isError: true, detail: detail)
            return nil
        case .read(let read):
            guard read.isVerified else {
                setStatus("The setup on \(setupHolder) does not reproduce the hash its agent reports, so it cannot be checked. Its agent has to be updated first.",
                          isError: true)
                return nil
            }
            if let error = read.verdict.errors.first {
                setStatus("The setup on \(setupHolder) breaks a rule every device holds a setup to, so it cannot be used: \(error.message)",
                          isError: true)
                return nil
            }
            return read
        }
    }

    func requestSetupReplacement() {
        guard case .conflict(let reason) = setupSharing else { return }
        ask?(PendingDialog(
            id: "replace-setup-\(historyMachineId)",
            title: "Replace the setup \(setupHolder) is holding?",
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
        let generation = core.agentGeneration
        perform("Replacing the setup on \(name)", kind: .shareSetup, subject: "setup") { _ in
            do {
                let result = try await self.configSet(local, replace: true)
                if result.ok == false {
                    let sentence = result.error ?? result.message ?? "the agent refused the document."
                    self.setStatus("The setup was not replaced: \(sentence)", isError: true)
                    return .init(state: .failed, summary: "The setup on \(self.setupHolder) was not replaced. \(sentence)")
                }
                // A push noted against an agent that has since been replaced is not the new one's.
                if self.core.agentGeneration == generation { self.core.setup.didReplace(with: local.hash) }
                self.setStatus("Replaced the setup \(self.setupHolder) was holding with this device's.")
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
    ///
    /// On a device that applies no setup yet this is enrolment, and nothing is applied until the
    /// person has seen every command the setup would have this device run and confirmed it. With a
    /// setup applied it is a compare-and-swap against the document applied when the read started.
    func adoptRemoteSetup() {
        let head = controllerConfig?()?.hash
        Task { @MainActor in
            defer { self.onStateChange?() }
            guard let theirs = await self.verifiedRemoteDocument() else { return }
            let taking = self.takeSetup?(theirs.bytes, theirs.identity, self.setupHolder, head)
                ?? .refused("The setup from \(self.setupHolder) has nowhere to go on this device.")
            switch taking {
            case .adopted:
                self.setStatus("This device now uses the setup from \(self.setupHolder).")
                self.core.setup.verdict = SetupSync.Verdict(.unknown)
            case .awaitingConfirmation:
                self.setStatus("The setup from \(self.setupHolder) is waiting for you to confirm what it runs. Nothing has been applied yet.")
            case .refused(let problem):
                self.setStatus(problem, isError: true)
            }
        }
    }
}
