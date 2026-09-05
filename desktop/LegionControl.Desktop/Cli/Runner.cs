using System.Text;
using System.Text.Json;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;
using LegionControl.Desktop.Updates;

namespace LegionControl.Desktop.Cli;

/// The app without a window.
///
/// Every command here goes through the same [AppModel], the same transport and the same decisions
/// the window uses. That is the whole point of it existing: a run against a real machine over ssh
/// exercises the code that ships, so a green result here is evidence about the app rather than
/// about a test harness that resembles it.
///
/// Exit codes are three, and the third is the one that matters:
///   0  it did what was asked, and that is known
///   1  it did not, and that is known
///   2  the outcome is not known - a link that went away, a machine that never answered a poll
/// Nothing here ever returns 0 for an outcome nobody observed.
public static class Runner
{
    public const int Ok = 0;
    public const int Failed = 1;
    public const int NotKnown = 2;

    private static readonly string[] ValueOptions =
    {
        "machine", "service", "action", "target", "op", "expires", "lines", "limit", "kind",
        "timeout", "out", "site", "pause", "windows", "automatic", "patience", "screenshot",
        "quit-after", "update-marker", "budget", "in", "self", "system", "local-agent",
        "identity", "device", "alias", "template", "command", "route", "trust-systems", "legacy-system", "base", "local-agent-json",
    };

    public static async Task<int> RunAsync(IReadOnlyList<string> arguments, TextWriter output)
    {
        if (arguments.Contains("-h")) { WriteUsage(output); return Ok; }
        var command = arguments.Count > 0 && !arguments[0].StartsWith("--", StringComparison.Ordinal)
            ? arguments[0]
            : arguments.FirstOrDefault(argument => argument == "--smoke") is not null
                ? "smoke"
                : null;

        var line = CommandLine.Parse(arguments, ValueOptions);
        if (line.Problem is { } problem)
        {
            output.WriteLine(problem);
            return Failed;
        }

        // `--command <verb>` and a bare verb are the same thing. The flag form exists because it
        // reads better in a scheduled task or a CI step.
        command = line.Value("command") ?? command ?? line.Positional.FirstOrDefault();
        if (line.Has("smoke")) command = "smoke";
        if (line.Has("reconcile-once")) command = "reconcile-once";
        if (line.Has("version")) command = "version";
        if (command is null || line.Has("help"))
        {
            WriteUsage(output);
            return command is null && !line.Has("help") ? Failed : Ok;
        }

        if (command == "version") return Version(output);
        var json = line.Has("json");
        using var app = new AppModel();
        using var cancellation = new CancellationTokenSource(
            TimeSpan.FromSeconds(line.Number("timeout") ?? 300));
        var token = cancellation.Token;

        try
        {
            return command switch
            {
                "smoke" => await SmokeAsync(app, line, output, json, token),
                "status" => await StatusAsync(app, line, output, json, token),
                "reconcile-once" => await ReconcileAsync(app, line, output, json, token),
                "setup" => await SetupAsync(app, line, output, json, token),
                "doctor" => await DoctorAsync(app, line, output, json, token),
                "logs" => await LogsAsync(app, line, output, token),
                "history" => await HistoryAsync(app, line, output, json, token),
                "bundle" => await BundleAsync(app, line, output, token),
                "op" => await OperationAsync(app, line, output, json, token),
                "cancel" => await CancelAsync(app, line, output, token),
                "policy" => await PolicyAsync(app, line, output, json, token),
                "wake" => await WakeAsync(app, line, output, token),
                "trust" => await TrustAsync(app, line, output, token),
                "agent-install" => await AgentInstallAsync(app, line, output, token),
                "app-update" => await AppUpdateAsync(app, line, output, token),
                "edit" => await EditAsync(app, line, output, token),
                "bindings" => Bindings(app, line, output),
                "templates" => Templates(output),
                "update" or "restart" or "run" or "boot" or "sleep" or "cycle" =>
                    await MutateAsync(app, command, line, output, json, token),
                "version" => Version(output),
                _ => Unknown(command, output),
            };
        }
        catch (OperationCanceledException)
        {
            output.WriteLine("The command ran out of time. Whether anything happened on the far side is not known.");
            return NotKnown;
        }
    }

    // MARK: reading

    /// Everything a person setting this up would want to check, in one pass and without changing
    /// anything: the document, this device's own settings, where it thinks it is, and what every
    /// machine answers.
    private static async Task<int> SmokeAsync(
        AppModel app, CommandLine line, TextWriter output, bool json, CancellationToken token)
    {
        var report = new List<object>();
        var worst = Ok;

        output.WriteLine($"Legion Control {AgentContract.ClientVersion}, contract {AgentContract.RequiredContract} required");
        output.WriteLine($"Setup document   {app.Config.Path}");
        output.WriteLine($"State directory  {AppPaths.Home}");
        output.WriteLine($"ssh              {AppPaths.SshBinary}"
                         + (AppPaths.KnownHostsOverride is { } known ? $", known_hosts {known}" : ""));
        output.WriteLine($"Trust key        {(Trust.HasKey ? Trust.KeyFingerprint : "MISSING - nothing signed can be verified")}");
        var (bundle, bundleProblem) = AgentBundle.Load();
        output.WriteLine($"Agent bundle     {(bundle is null ? bundleProblem : $"{bundle.Version}, verified against the release key")}");

        if (app.Config.Problem is { } problem)
        {
            output.WriteLine($"Setup problem    {problem}");
            worst = Failed;
        }
        if (app.Config.Document is { } document)
        {
            var identity = document.Identity;
            output.WriteLine($"Setup           {identity.Id ?? "no id"} revision {identity.RevisionNumber}, hash {document.Hash[..12]}, lineage {identity.Lineage.Count}");
        }
        foreach (var warning in app.Config.Warnings) output.WriteLine($"Setup warning    {warning}");
        output.WriteLine($"This device      {app.Bindings.Device}"
                         + (app.Bindings.Self is { } self ? $", bound to machine \"{self.Machine}\"" : ", not bound to a machine")
                         + (app.Bindings.LocalAgent.Count > 0 ? ", local agent configured" : ""));
        output.WriteLine($"Location         {app.Presence.Sentence}");
        output.WriteLine();

        var only = line.Value("machine");
        var machines = app.Machines.Where(machine => only is null || machine.Id == only).ToList();
        if (machines.Count == 0)
        {
            output.WriteLine(only is null ? "The setup lists no machines." : $"No machine called \"{only}\".");
            return Failed;
        }

        foreach (var machine in machines)
        {
            await machine.RefreshAsync(token);
            var entry = new Dictionary<string, object?> { ["machine"] = machine.Id };
            output.WriteLine($"{machine.Name} ({machine.Id})");
            if (machine.IsLocallyControlled) output.WriteLine("  controlled locally, no ssh");

            if (machine.Status is { } status)
            {
                output.WriteLine($"  answered as     {status.SystemName ?? status.SystemId ?? "an unnamed system"}"
                                 + $" over {machine.Routes.RememberedRouteId ?? "?"}");
                output.WriteLine($"  agent           {status.AgentVersion ?? "?"}, {machine.Capabilities.Summary}");
                if (status.Partial) output.WriteLine("  reading         partial: the agent ran out of budget");
                if (status.Config is { BlocksMutations: true }) output.WriteLine("  configuration   INVALID, the agent refuses every change");
                foreach (var service in status.Services)
                {
                    var busy = service.Busy?.Summary ?? "no busy state";
                    var update = service.HasUpdate switch
                    {
                        true => $"{service.Installed} -> {service.Latest}",
                        false => $"{service.Installed}, up to date",
                        null => $"{service.Installed ?? "?"}, newest version unknown",
                    };
                    output.WriteLine($"  service {service.DisplayName,-12} {update}; {busy}");
                }
                if (status.Operations.Running.Count > 0)
                {
                    foreach (var operation in status.Operations.Running)
                    {
                        output.WriteLine($"  running         {operation.Describe()} ({operation.Phase})");
                    }
                }
                foreach (var operation in status.Operations.Queued)
                {
                    output.WriteLine($"  queued          {operation.Describe()}, expires {operation.ExpiresAt:u}");
                }
                foreach (var note in status.Envelope.Notes) output.WriteLine($"  note            {note}");
                entry["ok"] = true;
                entry["contract"] = status.ContractVersion;
                if (!machine.Capabilities.SpeaksV3) worst = Math.Max(worst, Failed);
            }
            else if (machine.Failure is { } failure)
            {
                output.WriteLine($"  did not answer  {failure.Sentence(machine.Name)}");
                if (failure.Fix(machine.Machine.Routes.FirstOrDefault()?.Target.Host) is { } fix)
                {
                    output.WriteLine($"  fix             {fix}");
                }
                entry["ok"] = false;
                entry["failure"] = failure.ShortReason;
                worst = Math.Max(worst, failure.OutcomeUnknown ? NotKnown : Failed);
            }

            var plan = app.PlanWake(machine);
            if (machine.Machine.Wake is not null) output.WriteLine($"  wake            {plan.Sentence(machine.Name)}");
            report.Add(entry);
            output.WriteLine();
        }

        if (json) output.WriteLine(JsonSerializer.Serialize(report, Indented));
        return worst;
    }

    private static async Task<int> StatusAsync(
        AppModel app, CommandLine line, TextWriter output, bool json, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        await machine.RefreshAsync(token);
        if (machine.Status is not { } status)
        {
            output.WriteLine(machine.Failure?.Sentence(machine.Name) ?? "No reading.");
            return machine.Failure?.OutcomeUnknown == true ? NotKnown : Failed;
        }
        if (json)
        {
            output.WriteLine(JsonSerializer.Serialize(new
            {
                machine = machine.Id,
                system = status.SystemId,
                agentVersion = status.AgentVersion,
                contract = status.ContractVersion,
                partial = status.Partial,
                services = status.Services.Select(service => new
                {
                    id = service.Id,
                    installed = service.Installed,
                    latest = service.Latest,
                    running = service.IsRunning,
                    busy = service.Busy?.Summary,
                    monitored = service.Busy?.Monitored,
                }),
                controller = new { hash = status.Controller?.Hash, revision = status.Controller?.Revision },
            }, Indented));
            return Ok;
        }
        WriteStatus(output, machine, status);
        return Ok;
    }

    private static async Task<int> ReconcileAsync(
        AppModel app, CommandLine line, TextWriter output, bool json, CancellationToken token)
    {
        var outcomes = await app.ReconcileOnceAsync(token);
        var worst = Ok;
        foreach (var (id, outcome) in outcomes)
        {
            output.WriteLine($"{id,-16} {outcome.Sentence_}");
            if (outcome is ReconcileOutcome.NeedsDecision) worst = Math.Max(worst, Failed);
            if (outcome is ReconcileOutcome.Failed) worst = Math.Max(worst, Failed);
        }
        if (app.Config.Document is { } document)
        {
            output.WriteLine();
            output.WriteLine($"This device holds revision {document.Identity.RevisionNumber}, hash {document.Hash}");
        }
        if (json)
        {
            output.WriteLine(JsonSerializer.Serialize(
                outcomes.ToDictionary(entry => entry.Key, entry => entry.Value.Sentence_), Indented));
        }
        return worst;
    }

    /// The setup, and what each machine holds. Read-only unless one of the decision flags is given,
    /// and each of those is a decision a person made.
    private static async Task<int> SetupAsync(
        AppModel app, CommandLine line, TextWriter output, bool json, CancellationToken token)
    {
        var machine = line.Value("machine") is null ? null : Pick(app, line, output);

        if (line.Has("take-theirs") || line.Has("keep-mine") || line.Has("merge") || line.Has("replace-theirs"))
        {
            if (machine is null)
            {
                output.WriteLine("A decision needs --machine.");
                return Failed;
            }
            await machine.RefreshAsync(token);
            var problem = line switch
            {
                _ when line.Has("take-theirs") => await app.Reconciler.TakeTheirsAsync(machine, token),
                _ when line.Has("keep-mine") => await app.Reconciler.KeepMineAsync(machine, token),
                _ when line.Has("merge") => await app.Reconciler.MergeAsync(machine, null, token),
                _ => await app.Reconciler.ReplaceTheirsAsync(machine, token),
            };
            if (problem is not null)
            {
                output.WriteLine(problem);
                return Failed;
            }
            output.WriteLine($"Done. This device now holds revision {app.Config.Document?.Identity.RevisionNumber} ({app.Config.Document?.Hash[..12]}).");
            return Ok;
        }

        if (app.Config.Document is not { } document)
        {
            output.WriteLine(app.Config.Problem ?? "There is no setup document.");
            return Failed;
        }

        output.WriteLine($"Setup     {document.Identity.Id}");
        output.WriteLine($"Revision  {document.Identity.RevisionNumber} by {document.Identity.Device ?? "?"} ({document.Identity.Source ?? "?"})");
        output.WriteLine($"Hash      {document.Hash}");
        output.WriteLine($"Lineage   {(document.Identity.Lineage.Count == 0 ? "none" : string.Join(", ", document.Identity.Lineage.Select(hash => hash[..12])))}");
        output.WriteLine($"Kept      {app.Config.Ledger.Hashes().Count} earlier documents");

        foreach (var each in app.Machines.Where(each => machine is null || each.Id == machine.Id))
        {
            await each.RefreshAsync(token);
            var mark = each.Status?.Controller;
            output.WriteLine();
            output.WriteLine($"{each.Name,-16} {(mark?.Hash is { } hash ? hash[..12] : "nothing")}"
                             + (mark?.Revision is { } revision ? $" revision {revision}" : "")
                             + $"  {each.EvaluateSharing(document).Sentence_ ?? "unknown"}");
        }

        if (json) output.WriteLine(JsonSerializer.Serialize(new
        {
            id = document.Identity.Id,
            revision = document.Identity.RevisionNumber,
            hash = document.Hash,
            lineage = document.Identity.Lineage,
        }, Indented));
        return Ok;
    }

    private static async Task<int> DoctorAsync(
        AppModel app, CommandLine line, TextWriter output, bool json, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        await machine.RefreshAsync(token);
        var (report, problem) = await machine.DoctorAsync(line.Value("service"), line.Has("deep"), token);
        if (report is null)
        {
            output.WriteLine(problem ?? "No report.");
            return Failed;
        }
        foreach (var check in report.Checks)
        {
            output.WriteLine($"{check.Verdict.ToString().ToLowerInvariant(),-8} {check.DisplayName,-28} {check.Summary}");
            if (check.Verdict is DoctorVerdict.Failed or DoctorVerdict.Warning && check.Fix is { } fix)
            {
                output.WriteLine($"         fix: {fix}");
            }
        }
        if (json) output.WriteLine(JsonSerializer.Serialize(report.Checks, Indented));
        return report.Failures > 0 ? Failed : Ok;
    }

    private static async Task<int> LogsAsync(AppModel app, CommandLine line, TextWriter output, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        await machine.RefreshAsync(token);
        var (lines, problem) = await machine.LogsAsync(line.Number("lines") ?? 100, line.Value("op"), token);
        if (problem is not null)
        {
            output.WriteLine(problem);
            return Failed;
        }
        foreach (var text in lines) output.WriteLine(text);
        return Ok;
    }

    private static async Task<int> HistoryAsync(
        AppModel app, CommandLine line, TextWriter output, bool json, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        await machine.RefreshAsync(token);
        var (operations, problem) = await machine.HistoryAsync(
            line.Number("limit") ?? 30, line.Value("service"), line.Value("kind"), token);
        if (problem is not null)
        {
            output.WriteLine(problem);
            return Failed;
        }
        foreach (var operation in operations)
        {
            output.WriteLine($"{operation.UpdatedAt:u}  {operation.Describe(),-24} {operation.OutcomeText}"
                             + (operation.ReasonCode is { } reason ? $" ({reason})" : ""));
        }
        if (json) output.WriteLine(JsonSerializer.Serialize(operations, Indented));
        return Ok;
    }

    private static async Task<int> BundleAsync(AppModel app, CommandLine line, TextWriter output, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        await machine.RefreshAsync(token);
        var (text, problem) = await machine.BundleAsync(token);
        if (text is null)
        {
            output.WriteLine(problem ?? "No bundle.");
            return Failed;
        }
        var path = line.Value("out");
        if (path is null)
        {
            output.WriteLine(text);
            return Ok;
        }
        await File.WriteAllTextAsync(path, text, token);
        output.WriteLine($"Written to {path}. Values that looked like secrets were replaced before it was saved.");
        return Ok;
    }

    private static async Task<int> OperationAsync(
        AppModel app, CommandLine line, TextWriter output, bool json, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        var id = line.Value("op");
        if (id is null)
        {
            output.WriteLine("--op <id> is needed.");
            return Failed;
        }
        await machine.RefreshAsync(token);
        var tracked = await machine.ResolveAsync(id, token);
        if (tracked is null)
        {
            output.WriteLine($"This device has no record of {id}, and the machine was not asked about an id it never got from here.");
            return NotKnown;
        }
        output.WriteLine($"{tracked.Intent.Describe()}  {tracked.Outcome.ToString().ToLowerInvariant()}");
        if (tracked.Phase is { } phase) output.WriteLine($"phase   {phase}");
        if (tracked.Message is { } message) output.WriteLine($"message {message}");
        if (!json && tracked.Output is { } commandOutput) output.WriteLine(commandOutput);
        if (json) output.WriteLine(JsonSerializer.Serialize(tracked, Indented));
        return tracked.Outcome switch
        {
            OperationOutcome.Succeeded or OperationOutcome.Noop or OperationOutcome.Cancelled => Ok,
            OperationOutcome.Failed or OperationOutcome.Expired or OperationOutcome.Conflict => Failed,
            _ => NotKnown,
        };
    }

    private static async Task<int> CancelAsync(AppModel app, CommandLine line, TextWriter output, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        var id = line.Value("op");
        if (id is null)
        {
            output.WriteLine("--op <id> is needed.");
            return Failed;
        }
        await machine.RefreshAsync(token);
        var outcome = await machine.CancelAsync(id, token);
        output.WriteLine(outcome.Sentence_);
        return ExitFor(outcome);
    }

    private static async Task<int> PolicyAsync(
        AppModel app, CommandLine line, TextWriter output, bool json, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        await machine.RefreshAsync(token);
        var service = line.Value("service");

        if (line.Has("automatic") || line.Has("pause") || line.Has("resume") || line.Has("windows"))
        {
            var patch = new PolicyPatch();
            if (line.Value("automatic") is { } automatic)
            {
                patch = patch with
                {
                    SetAutomatic = true,
                    Automatic = automatic switch
                    {
                        "on" or "true" or "yes" => true,
                        "off" or "false" or "no" => false,
                        "inherit" or "null" => null,
                        _ => throw new ArgumentException($"--automatic takes on, off or inherit, not \"{automatic}\"."),
                    },
                };
            }
            if (line.Value("pause") is { } pause)
            {
                if (!TryDuration(pause, out var duration))
                {
                    output.WriteLine($"--pause takes a duration like 4h, not \"{pause}\".");
                    return Failed;
                }
                patch = patch with { SetPauseUntil = true, PauseUntil = DateTimeOffset.UtcNow + duration };
            }
            if (line.Has("resume")) patch = patch with { SetPauseUntil = true, PauseUntil = null };
            if (line.Value("windows") is { } windows)
            {
                var parsed = ParseWindows(windows, out var windowProblem);
                if (windowProblem is not null)
                {
                    output.WriteLine(windowProblem);
                    return Failed;
                }
                patch = patch with { SetMaintenanceWindows = true, MaintenanceWindows = parsed };
            }

            var (_, problem) = await machine.WritePolicyAsync(service, patch, token);
            if (problem is not null)
            {
                output.WriteLine(problem);
                return Failed;
            }
            output.WriteLine("Written.");
        }

        var (reply, readProblem) = await machine.ReadPolicyAsync(service, token);
        if (reply?.Effective is not { } policy)
        {
            output.WriteLine(readProblem ?? "No policy.");
            return Failed;
        }
        output.WriteLine($"automatic  {policy.Automatic?.ToString() ?? "inherited"}");
        output.WriteLine($"paused     {policy.PauseUntil?.ToString("u") ?? "no"}");
        output.WriteLine($"windows    {(policy.MaintenanceWindows is null ? "inherited" : policy.MaintenanceWindows.Count == 0 ? "any time" : string.Join("; ", policy.MaintenanceWindows.Select(window => window.Describe())))}");
        if (policy.EligibleNow is { } eligible) output.WriteLine($"eligible   {eligible}{(policy.DeferredReason is { } reason ? $" ({reason})" : "")}");
        if (json) output.WriteLine(JsonSerializer.Serialize(policy, Indented));
        return Ok;
    }

    private static async Task<int> WakeAsync(AppModel app, CommandLine line, TextWriter output, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        var plan = app.PlanWake(machine);
        output.WriteLine(plan.Sentence(machine.Name));
        foreach (var obstacle in plan.Obstacles) output.WriteLine($"  {obstacle}");
        foreach (var offer in plan.OfferToWakeFirst) output.WriteLine($"  \"{offer}\" could be woken first, as a separate step.");
        if (plan.IsEmpty) return Failed;
        if (!line.Has("yes"))
        {
            output.WriteLine("Add --yes to actually try it.");
            return Ok;
        }
        var report = await app.WakeAsync(machine,
            TimeSpan.FromSeconds(line.Number("patience") ?? 120), token);
        foreach (var attempt in report.Attempts) output.WriteLine($"  {attempt}");
        output.WriteLine(report.Sentence);
        return report.Confirmed ? Ok : NotKnown;
    }

    /// Reads the host keys a machine offers, and pins them only when told to.
    private static async Task<int> TrustAsync(AppModel app, CommandLine line, TextWriter output, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        var routes = app.Bindings.RoutesFor(machine.Machine);
        var routeId = line.Value("route");
        var route = routeId is null ? routes.FirstOrDefault() : routes.FirstOrDefault(candidate => candidate.Id == routeId);
        if (route is null)
        {
            output.WriteLine(routeId is null
                ? $"{machine.Name} has no address to check."
                : $"{machine.Name} has no route called \"{routeId}\". Its routes are: {string.Join(", ", routes.Select(candidate => candidate.Id))}");
            return Failed;
        }
        var resolved = await HostKeys.ForRouteAsync(new ProcessRunner(), route, app.Bindings.KnownHosts, token);
        if (resolved.Keys is not { } keys) { output.WriteLine(resolved.Problem); return Failed; }
        var pendingPath = Path.Combine(AppPaths.Home, "pending-host-approval.json");
        TrustOutcome outcome;
        if (line.Has("accept"))
        {
            try
            {
                var pending = JsonSerializer.Deserialize<HostKeyOffer>(await File.ReadAllTextAsync(pendingPath, token));
                if (pending is null || pending.Host != resolved.Host || pending.Port != resolved.Port)
                    throw new IOException("No displayed approval for this address. Run trust without --accept first.");
                outcome = new TrustOutcome.Offered(pending);
            }
            catch (Exception error) { output.WriteLine(error.Message); return Failed; }
        }
        else outcome = await keys.OfferAsync(resolved.Host, resolved.Port, token);
        switch (outcome)
        {
            case TrustOutcome.AlreadyPinned:
                output.WriteLine($"{route.Target.Host} is already in {keys.KnownHostsPath}. Nothing was changed.");
                return Ok;
            case TrustOutcome.Failed failed:
                output.WriteLine(failed.Sentence);
                return Failed;
            case TrustOutcome.Conflict conflict:
                output.WriteLine(conflict.Sentence);
                output.WriteLine(conflict.Fix);
                return Failed;
            case TrustOutcome.Offered offered:
                foreach (var fingerprint in offered.Offer.Fingerprints) output.WriteLine(fingerprint);
                if (!line.Has("accept"))
                {
                    Directory.CreateDirectory(AppPaths.Home);
                    await File.WriteAllTextAsync(pendingPath, JsonSerializer.Serialize(offered.Offer), token);
                    foreach (var existing in offered.Offer.ExistingFingerprints) output.WriteLine($"Existing pin: {existing}");
                    output.WriteLine("Verify these fingerprints, then use --accept --system OS. For initial local setup also confirm --trust-systems linux,windows; assign existing pins with --legacy-system OS (use GUI for separate assignments).");
                    return Ok;
                }
                if (line.Value("system") is not { } selected) { output.WriteLine("--system names the OS these keys belong to."); return Failed; }
                var groups = line.Value("trust-systems")?.Split(',', StringSplitOptions.RemoveEmptyEntries)
                    .Select(id => new HostSystemGroup(id.Trim(), id.Trim(), Array.Empty<HostPublicKey>())).ToList();
                var assignments = line.Value("legacy-system") is { } legacy
                    ? offered.Offer.ExistingKeys.ToDictionary(k => k, _ => legacy) : null;
                var pinned = await keys.PinAsync(offered.Offer, selected, groups, assignments, token);
                output.WriteLine(pinned switch
                {
                    TrustOutcome.Pinned added => $"Added {added.LinesAdded} key(s) to {keys.KnownHostsPath}.",
                    TrustOutcome.AlreadyPinned => "Something pinned it in the meantime. Nothing was changed.",
                    TrustOutcome.Failed problem => problem.Sentence,
                    TrustOutcome.Conflict conflict => conflict.Sentence,
                    _ => "Nothing was changed.",
                });
                return pinned is TrustOutcome.Pinned ? Ok : Failed;
            default:
                return Failed;
        }
    }

    private static async Task<int> AgentInstallAsync(AppModel app, CommandLine line, TextWriter output, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        await machine.RefreshAsync(token);
        if (!line.Has("yes"))
        {
            var (bundle, problem) = AgentBundle.Load();
            output.WriteLine(bundle is null
                ? problem!
                : $"This build carries agent {bundle.Version}, verified against the release key. {machine.Name} answers {machine.Capabilities.Summary}. Add --yes to install.");
            return bundle is null ? Failed : Ok;
        }
        var outcome = await new AgentInstaller(new ProcessRunner()).InstallAsync(machine, token, line.Value("base"));
        output.WriteLine(outcome.Sentence_);
        return outcome switch
        {
            AgentInstallOutcome.Installed => Ok,
            AgentInstallOutcome.NotKnown => NotKnown,
            _ => Failed,
        };
    }

    private static async Task<int> AppUpdateAsync(AppModel app, CommandLine line, TextWriter output, CancellationToken token)
    {
        var availability = await app.CheckForAppUpdateAsync(token);
        output.WriteLine(availability.Sentence_);
        if (availability is UpdateAvailability.Ready ready && line.Has("yes"))
        {
            var (bytes, downloadProblem) = await new AppUpdates().DownloadAsync(ready, token);
            if (bytes is null) { output.WriteLine(downloadProblem); return Failed; }
            var installer = new AppInstaller();
            var (staged, stageProblem) = installer.Stage(bytes, ready.Artifact.Name);
            if (staged is null) { output.WriteLine(stageProblem); return Failed; }
            var (started, failure) = installer.Apply();
            output.WriteLine(started ? "Verified update staged; the helper will replace and launch the app after this command exits. Completion is pending." : failure);
            return started ? NotKnown : Failed;
        }
        return availability is UpdateAvailability.Unavailable ? Failed : Ok;
    }

    /// Reads the document out to a file, or applies one back in.
    ///
    /// The same path the window's editor takes: applying gives the document the next revision and
    /// records what it came from, so every machine takes it as a step forward rather than as a
    /// disagreement.
    private static async Task<int> EditAsync(
        AppModel app, CommandLine line, TextWriter output, CancellationToken token)
    {
        if (line.Value("in") is { } input)
        {
            string text;
            try
            {
                text = await File.ReadAllTextAsync(input, token);
            }
            catch (Exception error)
            {
                output.WriteLine($"Could not read {input}: {error.Message}");
                return Failed;
            }
            var (document, problem) = app.Config.ApplyEdit(text, $"applied from {Path.GetFileName(input)}");
            if (document is null)
            {
                output.WriteLine(problem ?? "The document was refused.");
                return Failed;
            }
            output.WriteLine($"Applied as revision {document.Identity.RevisionNumber}, hash {document.Hash}.");
            foreach (var warning in app.Config.Warnings) output.WriteLine($"warning  {warning}");
            return Ok;
        }

        if (app.Config.Document is not { } current)
        {
            output.WriteLine(app.Config.Problem ?? "There is no setup document to read.");
            return Failed;
        }
        if (line.Value("out") is { } path)
        {
            await File.WriteAllTextAsync(path, current.Text, token);
            output.WriteLine($"Written to {path}. Edit it and apply it back with --command edit --in {path}");
            return Ok;
        }
        output.Write(current.Text);
        return Ok;
    }

    /// This device's own settings. Never published, and never part of the document.
    private static int Bindings(AppModel app, CommandLine line, TextWriter output)
    {
        var bindings = app.Bindings;
        var changed = false;

        if (line.Value("device") is { } device)
        {
            bindings = bindings with { DeviceName = device };
            changed = true;
        }
        if (line.Has("clear-self"))
        {
            bindings = bindings with { Self = null, LocalAgent = Array.Empty<string>() };
            changed = true;
        }
        if (line.Value("self") is { } self)
        {
            bindings = bindings with { Self = new SelfBinding(self, line.Value("system") ?? bindings.Self?.System) };
            changed = true;
        }
        if (line.Value("local-agent") is { } localAgent)
        {
            bindings = bindings with
            {
                LocalAgent = localAgent.Split(' ', StringSplitOptions.RemoveEmptyEntries),
            };
            changed = true;
        }
        if (line.Value("local-agent-json") is { } localJson)
        {
            try
            {
                var argv = JsonSerializer.Deserialize<string[]>(localJson);
                if (argv is null || argv.Length == 0 || argv.Any(string.IsNullOrEmpty))
                    throw new JsonException("Use a nonempty JSON array of argv strings.");
                bindings = bindings with { LocalAgent = argv };
                changed = true;
            }
            catch (JsonException error) { output.WriteLine(error.Message); return Failed; }
        }
        if (line.Value("identity") is { } identity)
        {
            bindings = bindings with { IdentityFile = identity };
            changed = true;
        }
        if (line.Value("site") is { } site)
        {
            bindings = bindings with { CurrentSite = site };
            changed = true;
        }
        if (line.Value("alias") is { } alias && line.Value("machine") is { } machineId)
        {
            var machines = bindings.Machines.ToDictionary(entry => entry.Key, entry => entry.Value, StringComparer.Ordinal);
            machines[machineId] = machines.TryGetValue(machineId, out var existing)
                ? existing with { SshAlias = alias }
                : new MachineBinding(null, alias);
            bindings = bindings with { Machines = machines };
            changed = true;
        }

        if (changed)
        {
            var problem = app.UpdateBindings(bindings);
            if (problem is not null)
            {
                output.WriteLine(problem);
                return Failed;
            }
        }

        output.WriteLine($"device        {app.Bindings.Device}");
        output.WriteLine($"self          {app.Bindings.Self?.Machine ?? "not bound to a machine"}"
                         + (app.Bindings.Self?.System is { } system ? $" ({system})" : ""));
        output.WriteLine($"local agent   {(app.Bindings.LocalAgent.Count == 0 ? "none: driven over ssh" : string.Join(" ", app.Bindings.LocalAgent))}");
        output.WriteLine($"key           {app.Bindings.IdentityFile ?? "whatever ssh decides"}");
        output.WriteLine($"known_hosts   {app.Bindings.KnownHosts ?? "the user's own"}");
        output.WriteLine($"site          {app.Bindings.CurrentSite ?? "not set"}");
        foreach (var (id, binding) in app.Bindings.Machines)
        {
            output.WriteLine($"  {id,-12} {binding.SshAlias ?? ""} {binding.IdentityFile ?? ""}".TrimEnd());
        }
        output.WriteLine($"stored in     {Config.Bindings.DefaultPath}");
        return Ok;
    }

    private static int Templates(TextWriter output)
    {
        foreach (var template in SetupTemplates.All())
        {
            output.WriteLine($"{template.Id,-20} {template.Kind,-9} {template.Name}");
            output.WriteLine($"                     {template.Summary}");
            output.WriteLine($"                     needs: {string.Join(", ", template.Fields.Select(field => field.Key))}");
        }
        var (_, problems) = SetupTemplates.UserTemplates();
        foreach (var problem in problems) output.WriteLine($"problem  {problem}");
        return Ok;
    }

    // MARK: changing

    private static async Task<int> MutateAsync(
        AppModel app, string verb, CommandLine line, TextWriter output, bool json, CancellationToken token)
    {
        var machine = Pick(app, line, output);
        if (machine is null) return Failed;
        await machine.RefreshAsync(token);
        if (line.Has("yes") && machine.Failure is { } statusFailure)
        {
            WriteOutcome(output, new CommandOutcome.Refused(statusFailure.Sentence(machine.Name), false, ReasonCode.NotConfigured), json);
            return Failed;
        }

        var request = new MachineRequest
        {
            Kind = verb switch
            {
                "update" => RequestKind.Update,
                "restart" => RequestKind.Restart,
                "run" => RequestKind.Run,
                "boot" => RequestKind.Boot,
                "sleep" => RequestKind.Sleep,
                _ => RequestKind.Cycle,
            },
            Service = line.Value("service"),
            ActionId = line.Value("action"),
            Target = line.Value("target"),
            Force = line.Has("force"),
            WhenIdle = line.Has("when-idle"),
            Expires = line.Value("expires"),
            DryRun = line.Has("dry-run"),
            OperationId = line.Value("op"),
        };

        var confirmation = Confirmation.For(request, machine.Status, machine.Name, machine.IsSelf);
        if (!line.Has("yes"))
        {
            output.WriteLine(confirmation.Title);
            output.WriteLine(confirmation.Body);
            if (confirmation.ForceWarning is { } warning) output.WriteLine(warning);
            output.WriteLine("Add --yes to go ahead" + (confirmation.NeedsForce ? ", and --force to go past the busy gate." : "."));
            return Ok;
        }
        if (confirmation.NeedsForce && !request.Force)
        {
            // Sent anyway: the agent is the one that decides, and its refusal carries the reason
            // this app would only be guessing at.
            output.WriteLine(confirmation.ForceWarning ?? "The machine may refuse this.");
        }

        var outcome = await machine.RequestAsync(request, token);
        WriteOutcome(output, outcome, json);
        return ExitFor(outcome);
    }

    public static void WriteOutcome(TextWriter writer, CommandOutcome outcome, bool json)
    {
        var commandOutput = outcome switch
        {
            CommandOutcome.Done done => done.Result.Output ?? done.Result.Operation?.Result?.Output,
            CommandOutcome.Following following => following.Operation.Output,
            CommandOutcome.Queued queued => queued.Operation.Output,
            _ => null,
        };
        writer.WriteLine(outcome.Sentence_);
        if (json) writer.WriteLine(JsonSerializer.Serialize(new { outcome = outcome.GetType().Name, sentence = outcome.Sentence_, output = commandOutput }, Indented));
        else if (commandOutput is not null) writer.WriteLine(commandOutput);
    }

    // MARK: plumbing

    private static readonly JsonSerializerOptions Indented = new() { WriteIndented = true };

    private static int ExitFor(CommandOutcome outcome) => outcome switch
    {
        CommandOutcome.Done done => done.Outcome switch
        {
            OperationOutcome.Succeeded or OperationOutcome.Noop or OperationOutcome.Cancelled => Ok,
            OperationOutcome.Pending or OperationOutcome.Unresolved => NotKnown,
            _ => Failed,
        },
        CommandOutcome.Queued or CommandOutcome.Following => Ok,
        CommandOutcome.NotKnown => NotKnown,
        _ => Failed,
    };

    private static MachineModel? Pick(AppModel app, CommandLine line, TextWriter output)
    {
        var id = line.Value("machine");
        if (id is not null)
        {
            var found = app.Machine(id);
            if (found is null) output.WriteLine($"No machine called \"{id}\". The setup has: {string.Join(", ", app.Machines.Select(machine => machine.Id))}");
            return found;
        }
        var machines = app.Machines;
        if (machines.Count == 1) return machines[0];
        output.WriteLine(machines.Count == 0
            ? $"The setup at {app.Config.Path} lists no machines."
            : $"--machine is needed. The setup has: {string.Join(", ", machines.Select(machine => machine.Id))}");
        return null;
    }

    private static void WriteStatus(TextWriter output, MachineModel machine, AgentStatus status)
    {
        output.WriteLine($"{machine.Name}  {status.SystemName ?? status.SystemId}");
        output.WriteLine($"agent      {status.AgentVersion}, contract {status.ContractVersion}");
        foreach (var service in status.Services)
        {
            output.WriteLine($"{service.DisplayName,-12} installed {service.Installed ?? "?"}"
                             + $"  latest {service.Latest ?? "unknown"}"
                             + $"  {(service.IsRunning == true ? "running" : "not running")}"
                             + $"  {service.Busy?.Summary ?? "no busy state"}");
        }
        if (status.Controller is { } mark)
        {
            output.WriteLine($"setup      {mark.Hash?[..12] ?? "none"} revision {mark.RevisionNumber} from {mark.Provenance}");
        }
    }

    private static bool TryDuration(string text, out TimeSpan duration)
    {
        duration = default;
        if (text.Length < 2) return false;
        if (!int.TryParse(text[..^1], out var amount)) return false;
        duration = text[^1] switch
        {
            'm' => TimeSpan.FromMinutes(amount),
            'h' => TimeSpan.FromHours(amount),
            'd' => TimeSpan.FromDays(amount),
            _ => TimeSpan.Zero,
        };
        return duration > TimeSpan.Zero;
    }

    /// `mon,tue,wed:02:00-06:00` and several of them separated by semicolons.
    public static IReadOnlyList<MaintenanceWindow> ParseWindows(string text, out string? problem)
    {
        problem = null;
        if (string.Equals(text, "none", StringComparison.OrdinalIgnoreCase)) return Array.Empty<MaintenanceWindow>();
        var windows = new List<MaintenanceWindow>();
        foreach (var part in text.Split(';', StringSplitOptions.RemoveEmptyEntries))
        {
            var pieces = part.Split(':', 2);
            if (pieces.Length != 2)
            {
                problem = $"\"{part}\" is not days:from-to, for example mon,tue:02:00-06:00.";
                return windows;
            }
            var hours = pieces[1].Split('-');
            if (hours.Length != 2)
            {
                problem = $"\"{part}\" needs a from and a to, separated by a dash.";
                return windows;
            }
            windows.Add(new MaintenanceWindow(
                pieces[0].Split(',', StringSplitOptions.RemoveEmptyEntries).Select(day => day.Trim().ToLowerInvariant()).ToList(),
                hours[0].Trim(),
                hours[1].Trim()));
        }
        return windows;
    }

    private static int Version(TextWriter output)
    {
        output.WriteLine($"Legion Control {AgentContract.ClientVersion}");
        output.WriteLine($"contract {AgentContract.RequiredContract}, bundled agent {AgentContract.BundledAgentVersion}");
        output.WriteLine($"release key {(Trust.HasKey ? Trust.KeyFingerprint : "missing")}");
        return Ok;
    }

    private static int Unknown(string command, TextWriter output)
    {
        output.WriteLine($"There is no command \"{command}\".");
        WriteUsage(output);
        return Failed;
    }

    private static void WriteUsage(TextWriter output) => output.WriteLine("""
        legion-control [--smoke | --command <verb> [options]]

        With no arguments it opens its window. Everything below runs without one and exits:
          0 it did what was asked, 1 it did not, 2 the outcome is not known.

        Reading, and changing nothing:
          --smoke [--machine ID] [--json]     read the setup and every machine, and say what is wrong
          --command status  [--machine ID] [--json]
          --command setup   [--machine ID]    what this device holds and what each machine holds
          --command doctor  [--machine ID] [--service ID] [--deep]
          --command logs    [--machine ID] [--lines N] [--op ID]
          --command history [--machine ID] [--limit N] [--service ID] [--kind K]
          --command bundle  [--machine ID] [--out FILE]
          --command op      --machine ID --op ID
          --command wake    --machine ID      what could wake it, in order; --yes to try
          --command trust   --machine ID [--route ID]  show the host keys; --accept to pin them
          --command edit    [--out FILE]      print or save the setup document
          --command bindings                  this device's own settings, which are never shared
          --command templates                 the shapes the editor can start a machine from
          --command version

        Changing something. Each of these needs --yes, and says what it would do without it:
          --command update  --machine ID [--service ID] [--force] [--when-idle] [--expires 4h]
          --command restart --machine ID [--service ID] [--force] [--when-idle]
          --command run     --machine ID --action ID [--force]
          --command boot    --machine ID --target ID [--force]
          --command sleep   --machine ID [--force]
          --command cycle   --machine ID [--dry-run]
          --command cancel  --machine ID --op ID
          --command policy  --machine ID [--service ID] [--automatic on|off|inherit]
                            [--pause 4h | --resume] [--windows "mon,tue:02:00-06:00" | --windows none]
          --command agent-install --machine ID [--yes]
          --command app-update

        Changing this device's own settings, or the setup document:
          --command bindings [--device NAME] [--self MACHINE [--system ID]] [--clear-self]
                             [--local-agent "node /path/to/agent/src/index.mjs"]
                             [--identity ~/.ssh/key] [--site ID] [--machine ID --alias NAME]
          --command edit --in FILE            apply an edited document as the next revision

        The setup, when peers disagree. Each is a decision, and none of them happens on its own:
          --reconcile-once                    one pass: publish where behind, adopt where ahead
                                              (also spelled --command reconcile-once)
          --command setup --machine ID --merge | --keep-mine | --take-theirs | --replace-theirs

        Where things live:
          LEGION_CONTROL_CONFIG  the setup document to read
          LEGION_CONTROL_HOME    everything this app writes, including its ssh material in test runs
          LEGION_CONTROL_SSH     the ssh binary to use
        """);
}
