using System.Net;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Model;

/// One way of getting a machine to wake up.
public abstract record WakeStep
{
    /// Send the packets from here. Only when this device is on the machine's network (section 10.1).
    public sealed record Direct(IReadOnlyList<string> Broadcast, IReadOnlyList<int> Ports, int Repeats) : WakeStep;
    /// Ask a machine that is on the target's network to send it. [ReadFirst] when this session has
    /// no status of the helper to go by: it is read once just before its step.
    public sealed record ViaHelper(WakeHelper Helper, string HelperName, bool ReadFirst) : WakeStep;

    public string Describe() => this switch
    {
        Direct direct => $"send the packet from here to {string.Join(", ", direct.Broadcast)}",
        ViaHelper helper => $"ask {helper.HelperName} to run {helper.Helper.Action}",
        _ => "",
    };
}

/// Why the direct step or one helper entry is not a step, with the sentence that says so.
///
/// [Kind] is `direct`, `helper`, or `helpers` for the plan with no helper at all; [Reason] is the
/// contract's word for it (contract section 10.2, `skipped` in contract/vectors/wake.json).
public sealed record WakeSkip(string Kind, string? Machine, string? Action, string Reason, string Sentence)
{
    public const string NoMac = "no-mac";
    public const string Missing = "missing";
    public const string Self = "self";
    public const string Duplicate = "duplicate";
    public const string Asleep = "asleep";
    public const string Unusable = "unusable";
    public const string NoAction = "no-action";
    public const string Held = "held";
    public const string None = "none";
}

/// What the last status read of a helper this session said, as the wake plan reads it.
public enum HelperReadiness
{
    /// Not read in this session, or the read ended some other way. Read once before its step.
    Unknown,
    /// It answered.
    Ready,
    /// The read found no route to it, or it did not answer.
    Asleep,
    /// The read failed in a way no wake can cure: a rejected key, a host key, a missing agent.
    Unusable,
    /// No system of it has a command this device approved (contract section 9.9).
    NotDriven,
}

/// One helper's last status read, and the kind of each action that status listed. [Actions] is
/// null when the status listed none, which says nothing about whether it has the action.
public sealed record HelperState(HelperReadiness Readiness, IReadOnlyDictionary<string, string?>? Actions = null)
{
    public static readonly HelperState Unread = new(HelperReadiness.Unknown);

    /// The mapping every client shares (S5.4): a reply is ready, no route or no answer is asleep, a
    /// failure no wake can cure is unusable, and any other ending is unknown and read again first.
    public static HelperState Of(MachineModel helper)
    {
        // A machine this device cannot drive on any system can never be read, so its last read is
        // no question at all.
        if (!helper.IsLocallyControlled
            && !helper.Machine.Systems.Any(system => system.LaunchState != LaunchState.NotDriven && system.Agent.Count > 0))
        {
            return new HelperState(HelperReadiness.NotDriven);
        }
        if (helper.Failure is { } failure)
        {
            return failure.Kind switch
            {
                FailureKind.NoRoute or FailureKind.HostUnreachable => new HelperState(HelperReadiness.Asleep),
                FailureKind.AuthenticationFailed or FailureKind.HostKeyChanged or FailureKind.HostKeyUnknown
                    or FailureKind.InterpreterMissing or FailureKind.AgentMissing or FailureKind.BadRequest => new HelperState(HelperReadiness.Unusable),
                _ => Unread,
            };
        }
        if (helper.Status is not { } status) return Unread;
        if (!status.ReportsActions) return new HelperState(HelperReadiness.Ready);
        var actions = new Dictionary<string, string?>(StringComparer.Ordinal);
        foreach (var action in status.Actions)
        {
            if (action.Id is { } id) actions.TryAdd(id, action.Kind);
        }
        return new HelperState(HelperReadiness.Ready, actions);
    }

    /// Whether the status declares this action a pure send of a magic packet, which is idempotent
    /// and the one kind of helper action a wake hold never stops.
    public bool IsWol(string action) => Actions is not null && Actions.TryGetValue(action, out var kind) && kind == "wol";
}

/// A wake hold, as the plan reads one: an action on a helper whose first run for this target may
/// still be going.
public sealed record WakeHold(string Helper, string Action);

/// What could be tried, in order, and what could not (contract section 10.2).
public sealed record WakePlan(
    IReadOnlyList<string> Matching,
    string? Effective,
    IReadOnlyList<WakeStep> Steps,
    IReadOnlyList<WakeSkip> Skipped,
    IReadOnlyList<string> OfferToWakeFirst)
{
    public bool IsEmpty => Steps.Count == 0;

    /// One sentence per entry the plan skipped, in plan order.
    public IReadOnlyList<string> Obstacles => Skipped.Select(skip => skip.Sentence).ToList();

    /// Whether a wake hold is what keeps a helper from running.
    public bool IsHeld => Skipped.Any(skip => skip.Reason == WakeSkip.Held);

    /// What to say when there is nothing to try. Never silence, and never a button that does
    /// nothing: a machine nobody can reach is a fact about the setup, and the setup is where it
    /// gets fixed.
    public string Sentence(string machineName) => IsEmpty
        ? $"There is no way to wake {machineName} from here. " + string.Join(" ", Obstacles)
        : $"{Steps.Count} way{(Steps.Count == 1 ? "" : "s")} to try: " + string.Join("; ", Steps.Select(step => step.Describe())) + ".";
}

/// Works out how a machine could be woken from this device, and in what order (reference
/// contract/tools/wake-plan.mjs `planWake`; contract/vectors/wake.json proves the ports agree).
///
/// The ordering is the whole content. A magic packet is a link-layer broadcast: it reaches a
/// machine only from something already on its network. So the first choice is this device when it
/// is on that network, the second is each configured helper in turn, and the third is saying
/// plainly that there is no way and why. A helper is never woken on the way to waking something
/// else: that would quietly turn the "no machine on all night" the user asked for into two
/// machines on all night.
public static class WakePlanner
{
    /// The limited broadcast address, the last resort of direct wake.
    public const string LimitedBroadcast = "255.255.255.255";

    /// The ports direct wake sends to when the wake block names none it can use.
    public static readonly IReadOnlyList<int> DefaultPorts = new[] { 9, 7 };

    /// Whether a broadcast entry is one a packet is sent to: a dotted IPv4 address. Nothing else is
    /// ever sent to, and nothing is ever resolved as a name.
    public static bool SendsTo(string entry) => LanPrefix.ParseIPv4(entry) is not null;

    public static WakePlan Plan(
        ControllerConfig config,
        string targetId,
        SitePresence presence,
        IReadOnlyDictionary<string, HelperState> helpers,
        IReadOnlyList<WakeHold> holds)
    {
        var target = config.Machine(targetId) ?? throw new ArgumentException($"the setup has no machine \"{targetId}\"", nameof(targetId));
        var wake = target.Wake;
        var steps = new List<WakeStep>();
        var skipped = new List<WakeSkip>();
        var offers = new List<string>();

        // 1. From here, but only when this device is on the target's network. Two houses behind
        //    two stock routers share a subnet, and a packet sent into the wrong one is silence that
        //    looks exactly like a packet sent into the right one. Without a hardware address there
        //    is no packet to send, and only the helpers are left.
        var place = wake?.Mac is null ? null : presence.PlaceOf(target);
        if (place is null)
        {
            skipped.Add(new WakeSkip("direct", null, null, WakeSkip.NoMac,
                $"{target.Name} names no hardware address, so no packet can wake it directly."));
        }
        else if (!place.On)
        {
            skipped.Add(new WakeSkip("direct", null, null, place.Reason!, place.Reason switch
            {
                NetworkPlace.Ambiguous => presence.Sentence,
                NetworkPlace.Unplaced => $"{target.Name} names no site and no LAN prefix, so whether this device is on its network cannot be told.",
                _ => $"This device is not on {target.Name}'s network.",
            }));
        }
        else
        {
            var site = target.Site is { } siteId ? config.Site(siteId) : null;
            steps.Add(new WakeStep.Direct(BroadcastsFor(wake!, site, place.Placing), PortsFor(wake!), WakeOnLan.Repeats));
        }

        // 2. Each helper in turn. A helper known to be asleep is not tried, but it is offered as
        //    something a person can wake first, which is the manual step the energy story needs.
        var list = wake?.EffectiveHelpers ?? Array.Empty<WakeHelper>();
        var earlier = new List<WakeHelper>();
        foreach (var entry in list)
        {
            var reason = SkipOf(config, targetId, entry, earlier, helpers, holds);
            earlier.Add(entry);
            var helperName = config.Machine(entry.Machine)?.Name ?? entry.Machine;
            if (reason is null)
            {
                steps.Add(new WakeStep.ViaHelper(entry, helperName, StateOf(helpers, entry.Machine).Readiness == HelperReadiness.Unknown));
                continue;
            }
            skipped.Add(new WakeSkip("helper", entry.Machine, entry.Action, reason, SkipSentence(reason, target.Name, entry, helperName)));
            if (reason == WakeSkip.Asleep && config.Machine(entry.Machine)?.Wake is not null && !offers.Contains(entry.Machine))
            {
                offers.Add(entry.Machine);
            }
        }
        if (list.Count == 0 && !steps.Any(step => step is WakeStep.Direct))
        {
            skipped.Add(new WakeSkip("helpers", null, null, WakeSkip.None, $"No helper is configured to wake {target.Name}."));
        }

        return new WakePlan(presence.Matching, presence.SiteId, steps, skipped, offers);
    }

    /// Why a helper entry is skipped, checked in this order, or null when it is a step: missing,
    /// self, duplicate, then from the helper's last status read asleep, unusable, no-action and held.
    private static string? SkipOf(
        ControllerConfig config, string targetId, WakeHelper entry, IReadOnlyList<WakeHelper> earlier,
        IReadOnlyDictionary<string, HelperState> helpers, IReadOnlyList<WakeHold> holds)
    {
        if (config.Machine(entry.Machine) is null) return WakeSkip.Missing;
        if (entry.Machine == targetId) return WakeSkip.Self;
        if (earlier.Any(other => other.Machine == entry.Machine && other.Action == entry.Action)) return WakeSkip.Duplicate;
        return SkipAtStep(StateOf(helpers, entry.Machine), entry.Action, holds.Count > 0);
    }

    /// The reasons that come from the helper's status, which the walk asks again at the step itself
    /// once an unread helper has been read and whenever a hold appeared since the plan was made.
    ///
    /// While any hold for the target exists, only a declared wol action runs, the held helper's
    /// included: a command whose first run may still be going must not get a second one somewhere
    /// else. A helper nobody has read cannot be shown to be exempt.
    public static string? SkipAtStep(HelperState state, string action, bool held)
    {
        if (state.Readiness == HelperReadiness.Asleep) return WakeSkip.Asleep;
        if (state.Readiness is HelperReadiness.Unusable or HelperReadiness.NotDriven) return WakeSkip.Unusable;
        var actions = state.Readiness == HelperReadiness.Ready ? state.Actions : null;
        if (actions is not null && !actions.ContainsKey(action)) return WakeSkip.NoAction;
        if (held && !state.IsWol(action)) return WakeSkip.Held;
        return null;
    }

    public static string SkipSentence(string reason, string targetName, WakeHelper entry, string helperName) => reason switch
    {
        WakeSkip.Missing => $"The helper \"{entry.Machine}\" is not a machine in this setup.",
        WakeSkip.Self => $"{targetName} lists itself as its own helper, which cannot work.",
        WakeSkip.Duplicate => $"{helperName} is listed again with {entry.Action}, and is asked only once.",
        WakeSkip.Asleep => $"{helperName} is asleep or off, so it cannot send the packet.",
        WakeSkip.Unusable => $"{helperName} cannot be driven from this device, so it was not asked.",
        WakeSkip.NoAction => $"{helperName} has no action called \"{entry.Action}\".",
        WakeSkip.Held => $"{helperName}'s {entry.Action} is not a declared wake action, and one run for {targetName} earlier may still be going, so it is not run again until that is known.",
        _ => $"{helperName} was not asked.",
    };

    private static HelperState StateOf(IReadOnlyDictionary<string, HelperState> helpers, string machine) =>
        helpers.TryGetValue(machine, out var state) ? state : HelperState.Unread;

    /// Where direct wake sends: the dotted entries of `wake.broadcast`; else those of the machine's
    /// site; else the directed broadcast of the placing address when its prefix length is known and
    /// at most 30; else 255.255.255.255. Guessing the limited broadcast first wakes nothing behind a
    /// router that drops it, which is why a site's own broadcast comes before it.
    private static IReadOnlyList<string> BroadcastsFor(WakeConfig wake, SiteConfig? site, LocalAddress? placing)
    {
        var listed = wake.Broadcast.Where(SendsTo).ToList();
        if (listed.Count > 0) return listed;
        var shared = site?.Broadcast.Where(SendsTo).ToList() ?? new List<string>();
        if (shared.Count > 0) return shared;
        if (placing is { PrefixLength: { } length and <= 30 } address)
        {
            // Every host bit set. A length of at most 30 keeps the shift inside the word.
            return new[] { LocalAddress.Dotted(address.Address | (uint.MaxValue >> length)) };
        }
        return new[] { LimitedBroadcast };
    }

    /// The ports of a wake block from 1 to 65535, or 9 and 7 when it names none of those.
    private static IReadOnlyList<int> PortsFor(WakeConfig wake)
    {
        var ports = wake.Ports.Where(port => port is >= 1 and <= 65535).Select(port => (int)port).ToList();
        return ports.Count > 0 ? ports : DefaultPorts;
    }
}

/// What came of trying to wake something.
public sealed record WakeReport(
    bool AnythingTried,
    string Sentence,
    IReadOnlyList<string> Attempts,
    bool Confirmed,
    string? SystemId)
{
    public static WakeReport NothingToTry(string sentence) =>
        new(false, sentence, Array.Empty<string>(), false, null);
}

/// Runs a wake plan and says honestly what happened (contract section 10.2, "The walk").
///
/// The one thing this will not do is claim a wake. Sending a magic packet proves nothing: nothing
/// acknowledges it, and a switch that drops broadcast traffic swallows it in silence. So the only
/// evidence accepted is an authenticated answer from the machine afterwards.
///
/// The other thing it will not do is run one command twice. A helper action that is not a declared
/// wol can do anything, and when its run ended without a terminal record it is held: the operation
/// carries the target in this device's own history, and until the hold is released no other
/// helper's command runs for that target, the held helper's own included.
public sealed class WakeRunner(OperationTracker tracker)
{
    private readonly OperationTracker _tracker = tracker;

    /// How long after a send the target is polled for.
    public const int ReadinessWindowMs = 120_000;
    private const int FirstPollMs = 5_000;
    private const int LongestIntervalMs = 20_000;

    /// The clock the readiness polls are timed by.
    public TimeProvider Time { get; init; } = TimeProvider.System;
    /// How the walk waits between readiness polls. A test passes one that moves its own clock.
    public Func<TimeSpan, CancellationToken, Task> Pause { get; init; } = Task.Delay;
    /// Where a packet goes. Null sends it from a broadcast socket; a test records it instead.
    public Action<byte[], IPEndPoint>? SendPacket { get; init; }

    /// When the readiness polls start, in milliseconds after the send: the first after 5 s, then
    /// at intervals growing by 8/5 up to 20 s, for as long as a poll starts within 120 s. A step that
    /// sent nothing waits for nothing.
    public static IReadOnlyList<int> ReadinessSchedule(bool sentAnything)
    {
        var starts = new List<int>();
        if (!sentAnything) return starts;
        var wait = FirstPollMs;
        for (var at = FirstPollMs; at <= ReadinessWindowMs; at += wait)
        {
            starts.Add(at);
            wait = Math.Min(wait * 8 / 5, LongestIntervalMs);
        }
        return starts;
    }

    public async Task<WakeReport> RunAsync(
        MachineModel target,
        WakePlan plan,
        IReadOnlyDictionary<string, MachineModel> machines,
        CancellationToken cancellationToken = default)
    {
        if (plan.IsEmpty) return WakeReport.NothingToTry(plan.Sentence(target.Name));

        var attempts = new List<string>();
        foreach (var step in plan.Steps)
        {
            cancellationToken.ThrowIfCancellationRequested();
            var sentAnything = step switch
            {
                WakeStep.Direct direct => SendDirect(target, direct, attempts),
                WakeStep.ViaHelper viaHelper => await AskHelperAsync(target, viaHelper, machines, attempts, cancellationToken),
                _ => false,
            };
            // Whatever was just tried, the only evidence that matters is the machine answering.
            if (sentAnything && await AnswersAsync(target, cancellationToken))
                return new WakeReport(true, $"{target.Name} is answering.", attempts, true, target.Status?.SystemId);
        }

        return new WakeReport(
            true,
            $"{target.Name} has not answered. A magic packet is never acknowledged, so whether it arrived is not known.",
            attempts.Concat(plan.Obstacles).ToList(),
            false,
            null);
    }

    private bool SendDirect(MachineModel target, WakeStep.Direct direct, List<string> attempts)
    {
        if (target.Machine.Wake?.MacBytes is not { } mac)
        {
            attempts.Add($"\"{target.Machine.Wake?.Mac}\" is not a six byte hardware address, so no packet went out.");
            return false;
        }
        var sent = WakeOnLan.Send(mac, direct.Broadcast, direct.Ports, direct.Repeats, SendPacket);
        attempts.Add(sent.Sentence);
        return sent.AnythingSent;
    }

    /// Runs one helper's action and says whether it finished `ran`. A helper skipped at its step, a
    /// failure before dispatch and any other ending move straight on to the next step.
    private async Task<bool> AskHelperAsync(
        MachineModel target, WakeStep.ViaHelper step, IReadOnlyDictionary<string, MachineModel> machines,
        List<string> attempts, CancellationToken cancellationToken)
    {
        if (!machines.TryGetValue(step.Helper.Machine, out var helper))
        {
            attempts.Add($"{step.HelperName}: not in this setup.");
            return false;
        }

        // A helper this session has no reading of is read once, now, and never more often than
        // any other machine. Whatever that read says is what the step goes by.
        var state = HelperState.Of(helper);
        if (step.ReadFirst || state.Readiness == HelperReadiness.Unknown)
        {
            await helper.RefreshAsync(cancellationToken, dialEveryRoute: true);
            state = HelperState.Of(helper);
            if (state.Readiness == HelperReadiness.Unknown) state = new HelperState(HelperReadiness.Unusable);
        }
        // A hold can also have appeared since the plan was made, from a helper earlier in this walk.
        if (WakePlanner.SkipAtStep(state, step.Helper.Action, _tracker.WakeHoldsFor(target.Id).Count > 0) is { } reason)
        {
            attempts.Add(WakePlanner.SkipSentence(reason, target.Name, step.Helper, step.HelperName));
            return false;
        }

        // A declared wol is a pure send and holds nothing. Anything else is recorded against the
        // target before it is sent, so a lost reply, a crash or a restart leaves the hold behind.
        var wol = state.IsWol(step.Helper.Action);
        var id = CommandSurface.NewOperationId();
        var outcome = await helper.RequestAsync(new MachineRequest
        {
            Kind = RequestKind.Run,
            ActionId = step.Helper.Action,
            OperationId = id,
            WakeTarget = wol ? null : target.Id,
        }, cancellationToken);
        attempts.Add($"{step.HelperName}: {outcome.Sentence_}");
        if (Ran(outcome)) return true;

        // Ended without a terminal record: on a contract 3 helper the record is read once more,
        // waiting for it, and a run that finished there counts as the run it was.
        if (!wol && _tracker.Find(id) is { IsWakeHold: true } && helper.Capabilities.SupportsOperationQuery)
        {
            var settled = await helper.ResolveAsync(id, cancellationToken, waitSeconds: 20, dialEveryRoute: true);
            if (settled is { State: OperationState.Finished, Outcome: OperationOutcome.Succeeded }) return true;
            if (settled is { IsWakeHold: true })
                attempts.Add($"{step.HelperName}: what became of {step.Helper.Action} is not known yet, so no other helper's command runs for {target.Name} until it is.");
        }
        return false;
    }

    private static bool Ran(CommandOutcome outcome) => outcome is CommandOutcome.Done { Outcome: OperationOutcome.Succeeded };

    /// The ordinary authenticated status poll of the target, at the readiness schedule from the
    /// send. Its addresses are dialled at every poll: a machine coming up is expected to fail the
    /// first ones, and leaving an address alone for a minute after each would let the window pass
    /// unread.
    private async Task<bool> AnswersAsync(MachineModel target, CancellationToken cancellationToken)
    {
        var sentAt = Time.GetUtcNow();
        foreach (var start in ReadinessSchedule(true))
        {
            var wait = sentAt + TimeSpan.FromMilliseconds(start) - Time.GetUtcNow();
            if (wait > TimeSpan.Zero) await Pause(wait, cancellationToken);
            await target.RefreshAsync(cancellationToken, dialEveryRoute: true);
            if (target.Status is not null && target.Failure is null) return true;
        }
        return false;
    }
}
