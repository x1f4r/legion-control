using System.Reflection;
using LegionControl.Desktop.Config;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Model;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Updates;

/// The signed agent bundle this build carries, if it carries one.
///
/// A normal build does not: the bundle is produced by the release tooling and embedded on the way
/// through. The action that installs an agent is then explicitly unavailable and says why, rather
/// than existing as a control that fails when pressed.
public sealed record AgentBundle(string Version, byte[] Archive, ReleaseManifest Manifest)
{
    public const string ManifestResource = "Legion-Control-agent-manifest.json";
    public const string SignatureResource = "Legion-Control-agent-manifest.json.sig";

    /// Reads and verifies the bundle. Verification happens here, once, before anything is uploaded
    /// anywhere: a bootstrap that verified on the far side would be trusting the machine it is
    /// trying to fix.
    public static (AgentBundle? Bundle, string? Problem) Load(Assembly? assembly = null)
    {
        assembly ??= Assembly.GetExecutingAssembly();
        var manifestBytes = ReadResource(assembly, ManifestResource);
        var signatureBytes = ReadResource(assembly, SignatureResource);

        if (manifestBytes is null || signatureBytes is null)
        {
            return (null, "This build carries no signed agent bundle, so it cannot install an agent.");
        }
        if (!Trust.HasKey)
        {
            return (null, "This build carries no release trust key, so the agent bundle cannot be verified or installed.");
        }

        var verdict = TrustVerdict.Check(manifestBytes, System.Text.Encoding.UTF8.GetString(signatureBytes));
        if (verdict is not TrustVerdict.Trusted trusted) return (null, verdict.Sentence_);

        var version = trusted.Manifest.AgentVersion;
        if (string.IsNullOrWhiteSpace(version))
        {
            return (null, "The signed manifest does not name an agent version, so there is nothing to install.");
        }

        var archiveResource = $"legionctl-agent-{version}.tgz";
        var archive = ReadResource(assembly, archiveResource);
        if (archive is null)
        {
            return (null, $"This build carries no {archiveResource}, so it cannot install agent {version}.");
        }

        var artifact = trusted.Manifest.Artifact(archiveResource);
        if (artifact is null)
        {
            return (null, $"The signed manifest does not list {archiveResource}, so there is nothing to install.");
        }
        if (!ReleaseManifest.Matches(artifact, archive))
        {
            return (null, $"The bundled {archiveResource} does not match its entry in the signed manifest. Nothing was installed.");
        }

        return (new AgentBundle(version, archive, trusted.Manifest), null);
    }

    private static byte[]? ReadResource(Assembly assembly, string name)
    {
        using var stream = assembly.GetManifestResourceStream(name);
        if (stream is null) return null;
        using var memory = new MemoryStream();
        stream.CopyTo(memory);
        return memory.ToArray();
    }
}

public abstract record AgentInstallOutcome
{
    public sealed record Installed(string Version, string Sentence) : AgentInstallOutcome;
    public sealed record Unavailable(string Sentence) : AgentInstallOutcome;
    public sealed record Failed(string Sentence, string? Detail) : AgentInstallOutcome;
    /// The upload or the install may or may not have happened. Never either of the other two.
    public sealed record NotKnown(string Sentence, string? Detail = null) : AgentInstallOutcome;

    public string Sentence_ => this switch
    {
        Installed installed => installed.Sentence,
        Unavailable unavailable => unavailable.Sentence,
        Failed failed => failed.Sentence,
        NotKnown notKnown => notKnown.Sentence,
        _ => "",
    };

    /// What the machine itself said, when it said something.
    public string? Detail_ => this switch
    {
        Failed failed => failed.Detail,
        NotKnown notKnown => notKnown.Detail,
        _ => null,
    };

    /// The sentence with what the machine said under it, which is how an install is reported. A
    /// remote "Permission denied" or a full disk is what tells somebody what to fix there, and the
    /// sentence alone cannot.
    public string Report => string.IsNullOrWhiteSpace(Detail_) ? Sentence_ : $"{Sentence_}\n{RemoteAgent.Condense(Detail_)}";
}

/// Puts a verified agent tree on a machine.
///
/// Two paths, and the difference is what the agent on the far side already understands. A contract
/// 3 agent reads the archive from its own standard input and verifies it again itself. An older one
/// has no such command, so the archive is written into its base directory, unpacked, and the new
/// tree is asked to install itself. Both paths verify this side first.
public sealed class AgentInstaller(IProcessRunner runner)
{
    private readonly IProcessRunner _runner = runner;

    /// [allowDowngrade] is a person's confirmed decision to go back: to put in a bundle older than
    /// the agent the machine runs, or below its version floor. Without it such a bundle is not
    /// installed at all, and with it the agent is told so in as many words (contract section 13).
    public async Task<AgentInstallOutcome> InstallAsync(
        MachineModel machine, CancellationToken cancellationToken = default, string? installationBase = null,
        bool allowDowngrade = false)
    {
        if (machine.Status is null || machine.Failure is not null)
            return new AgentInstallOutcome.Unavailable("Read an authenticated agent status before installing or upgrading it.");
        var (bundle, problem) = AgentBundle.Load();
        if (bundle is null) return new AgentInstallOutcome.Unavailable(problem ?? "No agent bundle.");
        var goesBack = GoesBack(bundle.Version, machine.Capabilities);
        if (goesBack is not null && !allowDowngrade)
            return new AgentInstallOutcome.Unavailable($"This build carries agent {bundle.Version}, {goesBack} on {machine.Name}, so it is not installed as an upgrade. Going back is a separate decision.");
        if (goesBack is not null && !machine.Capabilities.SupportsAllowDowngrade)
            return new AgentInstallOutcome.Unavailable(machine.Capabilities.RestrictedSession
                ? $"A restricted key can only move the agent on {machine.Name} forward. Go back to agent {bundle.Version} from an administrator SSH connection."
                : $"The agent on {machine.Name} reports no version floor, so it cannot be taken back deliberately from here. Install agent {bundle.Version} on it by hand.");

        var routes = machine.Bindings.RoutesFor(machine.Machine);
        var system = (machine.Routes.RememberedSystemId is { } id ? machine.Machine.System(id) : null)
                     ?? machine.Machine.Systems.FirstOrDefault();
        var route = routes.FirstOrDefault(candidate => candidate.Id == machine.Routes.RememberedRouteId)
                    ?? routes.FirstOrDefault();
        if (!machine.Bindings.CanRunLocally(machine.Id) && (system is null || route is null))
        {
            return new AgentInstallOutcome.Unavailable($"There is no way to reach {machine.Name}.");
        }

        var agent = machine.NewAgent();

        // The direct path: the agent reads the archive itself and verifies it against its own copy
        // of the trust key before it swaps anything.
        if (machine.Capabilities.SupportsAgentDeploy)
        {
            try
            {
                var reply = await agent.MutateAsync(
                    CommandSurface.SelfUpdateFromStdin(CommandSurface.NewOperationId(), machine.Capabilities, allowDowngrade: goesBack is not null),
                    TimeSpan.FromMinutes(5), system, route, bundle.Archive, cancellationToken);
                var result = reply.Value;
                if (result.Envelope.Ok == true)
                {
                    return await ConfirmInstalledAsync(machine, bundle.Version, cancellationToken);
                }
                if (result.Envelope.ReasonCode is ReasonCode.SignatureInvalid)
                {
                    return new AgentInstallOutcome.Failed(
                        $"{machine.Name} refused the bundle: its signature did not verify there. Nothing was installed.",
                        result.Envelope.Message);
                }
                return new AgentInstallOutcome.Failed(
                    result.Envelope.Sentence() ?? "The agent refused the bundle.", result.Output);
            }
            catch (AgentFailure failure)
            {
                if (failure.Dispatch != Dispatch.Never)
                {
                    return new AgentInstallOutcome.NotKnown(
                        $"{failure.Sentence(machine.Name)} Whether the agent was replaced is not known; read the agent version on the next status.");
                }
                return new AgentInstallOutcome.Failed(failure.Sentence(machine.Name), null);
            }
        }

        if (system is null || route is null || machine.Bindings.CanRunLocally(machine.Id))
            return new AgentInstallOutcome.Unavailable("Install the signed agent locally before using this device's local binding.");
        // The bootstrap runs the system's own interpreter over ssh, so it needs a command this
        // device approved as much as any agent command does.
        if (system.LaunchState == LaunchState.NotDriven)
            return new AgentInstallOutcome.Unavailable($"{system.Name} on {machine.Name} is not driven from this device until its command is approved.");
        if (machine.Status.AgentVersion is not { } version || !int.TryParse(version.Split('.')[0], out var major) || major >= 3)
            return new AgentInstallOutcome.Unavailable("Raw bootstrap requires an authenticated agent reporting a version older than 3.");
        if (system.Restricted || machine.Status.Agent?.RestrictedSession == true)
            return new AgentInstallOutcome.Unavailable("Raw bootstrap requires an administrator SSH connection.");
        return await BootstrapAsync(machine, bundle, system, route, cancellationToken, installationBase);
    }

    /// The path for an agent that cannot read an archive itself: write the file, unpack it, and let
    /// the new tree install itself over the old one.
    private async Task<AgentInstallOutcome> BootstrapAsync(
        MachineModel machine, AgentBundle bundle, SystemConfig system, MachineRoute route,
        CancellationToken cancellationToken, string? installationBase)
    {
        if (system.Grammar.Shell != RemoteShell.Posix)
        {
            return new AgentInstallOutcome.Unavailable(
                $"{machine.Name} runs an agent too old to be replaced from here, and this app only bootstraps POSIX systems. Install agent {bundle.Version} on that system by hand.");
        }

        var basePath = ResolveBase(machine, system) ?? installationBase;
        if (basePath is null)
        {
            return new AgentInstallOutcome.Unavailable(
                $"Where the agent lives on {machine.Name} could not be worked out from the setup document, so nothing was sent.");
        }
        if (!basePath.StartsWith('/') || basePath.Trim('/').Length == 0)
            return new AgentInstallOutcome.Unavailable("Use an absolute installation directory for this POSIX agent.");

        var raw = new RawSsh(_runner);
        var archivePath = $"{basePath}/incoming/legionctl-agent-{bundle.Version}.tgz";
        var upload = await raw.UploadAsync(route, system, archivePath, bundle.Archive,
            TimeSpan.FromMinutes(5), cancellationToken);
        if (!upload.Succeeded)
        {
            return Unfinished(upload, $"The archive could not be written to {machine.Name}.",
                $"The upload to {machine.Name} did not finish cleanly. Whether anything was written there is not known.");
        }

        var unpack = await raw.RunAsync(route, system,
            new[] { "tar", "-xzf", archivePath, "-C", $"{basePath}/incoming" },
            TimeSpan.FromMinutes(2), cancellationToken);
        if (!unpack.Succeeded)
        {
            return new AgentInstallOutcome.Failed($"The archive could not be unpacked on {machine.Name}.", unpack.FailureText);
        }

        // The staged tree installs itself, and verifies its own manifest on the way. The running
        // agent is too old to be asked to do it.
        var interpreter = system.Agent.FirstOrDefault() ?? "node";
        var install = await raw.RunAsync(route, system,
            new[] { interpreter, $"{basePath}/incoming/agent/src/index.mjs", "self-update", "--install" },
            TimeSpan.FromMinutes(5), cancellationToken);

        var json = install.Reading.IsReply ? AttemptClassifier.JsonObjectSlice(install.Result.StandardOutput) : null;
        if (json is null)
        {
            // A dropped link or a timeout may have left it halfway through installing itself.
            return Unfinished(install, $"The staged agent on {machine.Name} did not install.",
                $"The install on {machine.Name} did not answer. Read the agent version on the next status to find out what happened.");
        }

        var envelope = AgentEnvelope.From(Value.Parse(json));
        if (envelope.Ok == true)
        {
            return await ConfirmInstalledAsync(machine, bundle.Version, cancellationToken);
        }
        return new AgentInstallOutcome.Failed(
            envelope.Sentence() ?? $"The staged agent on {machine.Name} refused to install itself.", null);
    }

    /// How installing agent [version] would take the machine back, or null when it would not: older
    /// than the agent it runs, or below its version floor. Only versions known to be older count; a
    /// version that cannot be read is not taken for an older one.
    public static string? GoesBack(string version, AgentCapabilities capabilities) =>
        AgentVersions.IsOlder(version, capabilities.AgentVersion)
            ? $"older than the {capabilities.AgentVersion} it runs"
            : AgentVersions.IsOlder(version, capabilities.Floor)
                ? $"below the version floor {capabilities.Floor}"
                : null;

    /// What a bootstrap step that did not finish means. A failure only when it provably never
    /// ran; anything else may have done some or all of its work, and is not known. Either way what
    /// the machine said goes with it.
    public static AgentInstallOutcome Unfinished(RawSshResult step, string failed, string notKnown) =>
        step.NeverRan
            ? new AgentInstallOutcome.Failed(failed, step.FailureText)
            : new AgentInstallOutcome.NotKnown(notKnown, step.FailureText);

    private static async Task<AgentInstallOutcome> ConfirmInstalledAsync(MachineModel machine, string version, CancellationToken token)
    {
        await machine.RefreshAsync(token);
        return machine.Failure is null && machine.Status?.AgentVersion == version
            ? new AgentInstallOutcome.Installed(version, $"Agent {version} installed on {machine.Name}; authenticated status confirmed the version.")
            : new AgentInstallOutcome.NotKnown($"The installer acknowledged the upgrade, but authenticated status has not confirmed agent {version}. Read status again before changing the launcher path.");
    }

    /// Where the agent lives on the far side.
    ///
    /// The agent says so itself from contract 3 onwards. Before that it is worked out from the
    /// script path in the setup document, which is `<base>/agent/src/index.mjs` in every installed
    /// layout, and nothing at all when the document names something else: a guess about where to
    /// write a file is not something to make on somebody's machine.
    public static string? ResolveBase(MachineModel machine, SystemConfig system)
    {
        if (machine.Capabilities.SpeaksV3 && machine.Status?.Agent?.Base is { Length: > 0 } reported) return reported.TrimEnd('/');
        var script = system.Agent.LastOrDefault(part => part.EndsWith("index.mjs", StringComparison.Ordinal)
            || part.EndsWith("launcher.mjs", StringComparison.Ordinal));
        if (script is null) return null;
        var normalised = script.Replace('\\', '/');
        var parts = normalised.Split('/');
        if (parts.Length >= 3 && parts[^2] == "bin" && parts[^1] == "launcher.mjs")
            return string.Join("/", parts[..^2]).TrimEnd('/');
        if (parts.Length < 4) return null;
        if (parts[^2] != "src" || parts[^3] != "agent" || parts[^1] != "index.mjs") return null;
        return string.Join("/", parts[..^3]).TrimEnd('/');
    }
}
