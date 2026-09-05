using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Model;

/// One way of getting a machine to wake up.
public abstract record WakeStep
{
    /// Send the packet from here. Only when this device is demonstrably on the machine's network.
    public sealed record Direct(IReadOnlyList<string> Broadcast, IReadOnlyList<int> Ports) : WakeStep;
    /// Ask a machine that is on the target's network to send it.
    public sealed record ViaHelper(WakeHelper Helper, string HelperName) : WakeStep;

    public string Describe() => this switch
    {
        Direct direct => $"send the packet from here to {string.Join(", ", direct.Broadcast)}",
        ViaHelper helper => $"ask {helper.HelperName} to run {helper.Helper.Action}",
        _ => "",
    };
}

/// What could be tried, in order, and what could not.
public sealed record WakePlan(
    IReadOnlyList<WakeStep> Steps,
    IReadOnlyList<string> Obstacles,
    IReadOnlyList<string> OfferToWakeFirst)
{
    public bool IsEmpty => Steps.Count == 0;

    /// What to say when there is nothing to try. Never silence, and never a button that does
    /// nothing: a machine nobody can reach is a fact about the setup, and the setup is where it
    /// gets fixed.
    public string Sentence(string machineName) => IsEmpty
        ? $"There is no way to wake {machineName} from here. " + string.Join(" ", Obstacles)
        : $"{Steps.Count} way{(Steps.Count == 1 ? "" : "s")} to try: " + string.Join("; ", Steps.Select(step => step.Describe())) + ".";
}

/// Works out how a machine could be woken from this device, and in what order.
///
/// The ordering is the whole content. A magic packet is a link-layer broadcast: it reaches a
/// machine only from something already on its network. So the first choice is this device when it
/// is provably on that network, the second is each configured helper in turn, and the third is
/// saying plainly that there is no way and why. A helper is never woken on the way to waking
/// something else: that would quietly turn the "no machine on all night" the user asked for into
/// two machines on all night.
public static class WakePlanner
{
    public static WakePlan Plan(
        MachineConfig target,
        ControllerConfig config,
        SitePresence presence,
        IReadOnlyDictionary<string, MachineModel> machines,
        IReadOnlyList<string>? localAddresses = null)
    {
        var steps = new List<WakeStep>();
        var obstacles = new List<string>();
        var offers = new List<string>();

        if (target.Wake is not { } wake)
        {
            return new WakePlan(steps, new[] { $"{target.Name} has no wake configuration." }, offers);
        }
        if (wake.MacBytes is null)
        {
            return new WakePlan(steps, new[] { $"\"{wake.Mac}\" is not a six byte hardware address." }, offers);
        }

        // 1. From here, but only when being on that network is more than a guess. Two houses behind
        //    two stock routers share a subnet, and a packet sent into the wrong one is silence that
        //    looks exactly like a packet sent into the right one.
        if (presence.IsOnNetworkOf(target, localAddresses))
        {
            var broadcast = wake.Broadcast.Count > 0
                ? wake.Broadcast
                : config.Site(target.Site ?? "")?.Broadcast ?? Array.Empty<string>();
            if (broadcast.Count > 0) steps.Add(new WakeStep.Direct(broadcast, wake.Ports));
            else obstacles.Add($"{target.Name} names no broadcast address to send to.");
        }
        else if (presence.IsAmbiguous)
        {
            obstacles.Add(presence.Sentence);
        }

        // 2. Each helper in turn. A helper that is known to be asleep is not tried, but it is
        //    offered as something a person can wake first, which is the manual step the energy
        //    story needs.
        foreach (var helper in wake.EffectiveHelpers)
        {
            if (helper.Machine == target.Id)
            {
                obstacles.Add($"{target.Name} lists itself as its own helper, which cannot work.");
                continue;
            }
            if (config.Machine(helper.Machine) is not { } helperMachine)
            {
                obstacles.Add($"The helper \"{helper.Machine}\" is not a machine in this setup.");
                continue;
            }
            machines.TryGetValue(helper.Machine, out var model);
            if (model is { Failure: { MeansAsleepOrOff: true } })
            {
                obstacles.Add($"{helperMachine.Name} is asleep or off, so it cannot send the packet.");
                if (helperMachine.Wake is not null) offers.Add(helper.Machine);
                continue;
            }
            if (model?.Status is { } status && status.Actions.All(action => action.Id != helper.Action))
            {
                obstacles.Add($"{helperMachine.Name} has no action called \"{helper.Action}\".");
                continue;
            }
            steps.Add(new WakeStep.ViaHelper(helper, helperMachine.Name));
        }

        if (steps.Count == 0 && obstacles.Count == 0)
        {
            obstacles.Add(
                $"This device is not on {target.Name}'s network and no helper is configured for it.");
        }
        return new WakePlan(steps, obstacles, offers);
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

/// Runs a wake plan and says honestly what happened.
///
/// The one thing this will not do is claim a wake. Sending a magic packet proves nothing: nothing
/// acknowledges it, and a switch that drops broadcast traffic swallows it in silence. So the only
/// evidence accepted is an authenticated answer from the machine afterwards.
public sealed class WakeRunner(IProcessRunner runner)
{
    private readonly IProcessRunner _runner = runner;

    /// How often the target may be probed while it comes up. A machine that is booting has better
    /// things to do, and a faster probe on a private network looks like a scan.
    public static readonly TimeSpan ProbeInterval = TimeSpan.FromSeconds(3);

    public async Task<WakeReport> RunAsync(
        MachineModel target,
        WakePlan plan,
        IReadOnlyDictionary<string, MachineModel> machines,
        TimeSpan patience,
        CancellationToken cancellationToken = default)
    {
        if (plan.IsEmpty) return WakeReport.NothingToTry(plan.Sentence(target.Name));

        var attempts = new List<string>();
        foreach (var step in plan.Steps)
        {
            cancellationToken.ThrowIfCancellationRequested();
            switch (step)
            {
                case WakeStep.Direct direct:
                    var sent = WakeOnLan.Send(target.Machine.Wake!, direct.Broadcast, direct.Ports);
                    attempts.Add(sent.Sentence);
                    break;

                case WakeStep.ViaHelper viaHelper:
                    if (!machines.TryGetValue(viaHelper.Helper.Machine, out var helper))
                    {
                        attempts.Add($"{viaHelper.HelperName}: not in this setup.");
                        continue;
                    }
                    var outcome = await helper.RequestAsync(
                        new MachineRequest { Kind = RequestKind.Run, ActionId = viaHelper.Helper.Action },
                        cancellationToken);
                    attempts.Add($"{viaHelper.HelperName}: {outcome.Sentence_}");

                    // An action whose outcome is unknown is not retried anywhere else. A declared
                    // wake action is a packet and repeating it is harmless, but a general command
                    // action could be anything, and this app cannot tell a second helper to do
                    // something when it does not know whether the first one already did.
                    if (outcome is CommandOutcome.NotKnown)
                    {
                        var reason = IsDeclaredWake(helper, viaHelper.Helper.Action)
                            ? null
                            : $"{viaHelper.HelperName} did not say what became of {viaHelper.Helper.Action}, and it is not a declared wake action, so no other helper was asked.";
                        if (reason is not null)
                        {
                            return new WakeReport(true, reason, attempts, false, null);
                        }
                    }
                    break;
            }

            // Whatever was just tried, the only evidence that matters is the machine answering.
            var confirmed = await WaitForAnswerAsync(target, patience, cancellationToken);
            if (confirmed) return new WakeReport(true, $"{target.Name} is answering.", attempts, true, target.Status?.SystemId);
        }

        return new WakeReport(
            true,
            $"{target.Name} has not answered. A magic packet is never acknowledged, so whether it arrived is not known.",
            attempts,
            false,
            null);
    }

    /// Whether the target's own agent answers, which is the only thing that proves it is up.
    private static async Task<bool> WaitForAnswerAsync(MachineModel target, TimeSpan patience, CancellationToken cancellationToken)
    {
        var deadline = DateTimeOffset.UtcNow + patience;
        while (DateTimeOffset.UtcNow < deadline)
        {
            cancellationToken.ThrowIfCancellationRequested();
            await target.RefreshAsync(cancellationToken);
            if (target.Status is not null && target.Failure is null) return true;
            await Task.Delay(ProbeInterval, cancellationToken);
        }
        return false;
    }

    private static bool IsDeclaredWake(MachineModel helper, string actionId) =>
        helper.Status?.Actions.FirstOrDefault(action => action.Id == actionId)?.IsWake == true;
}
