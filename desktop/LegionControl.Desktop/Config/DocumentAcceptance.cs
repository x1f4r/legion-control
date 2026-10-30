using System.Globalization;
using System.Text;
using System.Text.RegularExpressions;
using LegionControl.Desktop.Contract;
using LegionControl.Desktop.Transport;

namespace LegionControl.Desktop.Config;

/// Where a document comes from, which decides what happens to one that fails (contract section
/// 9.7). A peer document (read from a machine, a kept revision, a merge result) and a local one
/// (typed, pasted, opened from a file) are refused. The applied document, the one this device
/// already runs on, is kept with its errors shown as long as section 12 accepts its bytes.
public enum Intake
{
    Peer,
    Local,
    Applied,
}

/// One thing section 9.7 says about a document: its code, where it is, and a sentence for a person.
public sealed record DocumentProblem(string Code, string Path, string Message);

/// What section 9.7 says about one document under one intake.
public sealed record Acceptance(
    bool Valid,
    IReadOnlyList<DocumentProblem> Errors,
    IReadOnlyList<DocumentProblem> Warnings,
    bool AssignsIdentity,
    bool Keep)
{
    /// The machines an error is inside, by their index in `machines`. Under applied intake each of
    /// them is left out of what this device dials and runs; the rest of the document still works.
    public IReadOnlySet<int> BrokenMachines
    {
        get
        {
            var broken = new HashSet<int>();
            foreach (var error in Errors)
            {
                var match = MachinePath.Match(error.Path);
                if (match.Success) broken.Add(int.Parse(match.Groups[1].Value, CultureInfo.InvariantCulture));
            }
            return broken;
        }
    }

    /// Every error, as one paragraph for the setup page.
    public string? Sentence => Errors.Count == 0 ? null : string.Join(" ", Errors.Select(error => error.Message));

    private static readonly Regex MachinePath = new(@"\Amachines\[([0-9]+)\](?:\.|\[|\z)", RegexOptions.CultureInvariant);
}

/// Which setup documents this client accepts: exactly the ones `node contract/validate.mjs` accepts,
/// no more and no fewer (contract section 9.7, reference contract/tools/document-rules.mjs).
///
/// Before this every client had its own answer. This one counted endpoints without an id as
/// duplicates of each other, refused a wake block with no hardware address, refused a site name in
/// a document with no sites, and checked setup ids with the generic token grammar, so it took ids
/// the agent refuses and refused documents a peer had every right to write. A client that
/// refuses its peer's document stops replicating without anybody noticing.
///
/// So a document is refused for a broken schema constraint or one of the relationship rules, and
/// for nothing else. Whatever this app cannot use is a warning with a code, so it can say why a
/// machine is not driven without refusing the setup it is in. The schema is written out by hand
/// because the reference does the same and is held to the schema case by case; the shared vectors
/// hold this file to the reference.
public static class DocumentAcceptance
{
    /// The setup id the check gives a local document that has none. The store mints a fresh one when
    /// it actually applies the document; any valid id gives the same verdict.
    public const string AssignedSetupId = "setup-00000000-0000-4000-8000-000000000000";

    /// The most UTF-8 bytes one agent argv item may be, which a schema cannot count.
    public const int ArgItemBytes = 4096;

    /// The most machines a document may list.
    public const int MaxMachines = 50;

    private static readonly string[] Sources = { "mac", "desktop", "phone", "cli", "legacy" };
    private static readonly string[] PlatformNames = { "linux", "windows", "mac" };
    private static readonly string[] ShellNames = { "posix", "powershell", "cmd" };
    private static readonly string[] Kinds = { "lan", "remote" };

    // The string guards of common.schema.json and controller-document.schema.json, matched the way
    // every implementation matches them: the whole input, ASCII classes, and a line feed or a
    // carriage return refused before any pattern sees the value.
    private static readonly Regex HostGuard = new(
        @"\A(?:[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?(?:\.[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?)*\.?|[0-9A-Fa-f.]*:[0-9A-Fa-f:.]*(?:%[A-Za-z0-9._-]{1,32})?)\z",
        RegexOptions.CultureInvariant);
    private static readonly Regex UserGuard = new(
        @"\A(?=[^\n]{1,64}\z)(?!.* [ -])[A-Za-z0-9_](?:[A-Za-z0-9._@\\ -]*[A-Za-z0-9._-])?\z", RegexOptions.CultureInvariant);
    private static readonly Regex ArgItem = new(@"\A[^\u0000-\u001f\u007f]+\z", RegexOptions.CultureInvariant);
    private static readonly Regex MacAddress = new(@"\A[0-9A-Fa-f]{2}(?:[:-][0-9A-Fa-f]{2}){5}\z", RegexOptions.CultureInvariant);

    public static Acceptance Check(string text, Intake intake) => Check(Encoding.UTF8.GetBytes(text), intake);

    /// Section 9.7 for a document's bytes under one intake. The order is fixed. Section 12 comes
    /// first: a text it refuses is no document at all, so it is a `schema` error under every
    /// intake and not even applied intake keeps it. Then a repeated member name, on the canonical
    /// text, which when it applies is the only thing reported. Then local intake's identity, then
    /// the schema and the rules. Warnings are only reported for a document without errors.
    public static Acceptance Check(byte[] bytes, Intake intake)
    {
        if (Canonical.Rejection(bytes) is { } refused)
        {
            return new Acceptance(false,
                new[] { new DocumentProblem("schema", "", $"The setup is refused before it is read ({refused}), so there is no document to check.") },
                Array.Empty<DocumentProblem>(), AssignsIdentity: false, Keep: false);
        }
        var document = DocumentJson.Read(Encoding.UTF8.GetString(Canonical.Bytes(bytes)), out var repeated);
        if (repeated is not null)
        {
            return new Acceptance(false,
                new[] { new DocumentProblem("duplicate-member", repeated, $"{repeated} is written twice in one object, and parsers disagree about which copy counts.") },
                Array.Empty<DocumentProblem>(), AssignsIdentity: false, Keep: intake == Intake.Applied);
        }
        var assignsIdentity = intake == Intake.Local && NeedsIdentity(document);
        if (assignsIdentity) document = WithIdentity((DocumentObject)document);
        var (errors, warnings) = Check(document);
        var valid = errors.Count == 0;
        return new Acceptance(valid, errors, warnings, assignsIdentity, intake == Intake.Applied || valid);
    }

    /// Whether local intake gives this document an identity before it is checked: when it has no
    /// `controller` block, or a block whose id is absent or null (contract section 9.5).
    public static bool NeedsIdentity(DocumentValue document)
    {
        if (document is not DocumentObject root || root["controller"] is not { } block) return true;
        return block is DocumentObject controller && controller["id"] is null or DocumentLiteral { Value: null };
    }

    private static DocumentObject WithIdentity(DocumentObject document)
    {
        var block = document["controller"] is DocumentObject existing ? (DocumentObject)existing.Clone() : new DocumentObject();
        block.Set("id", new DocumentString(AssignedSetupId));
        block.Set("revision", new DocumentNumber("1"));
        block.Set("lineage", new DocumentArray());
        var copy = (DocumentObject)document.Clone();
        copy.Set("controller", block);
        return copy;
    }

    /// The errors and warnings of a parsed document under peer intake. A schema problem and a
    /// relationship problem are both errors; warnings are only reported for a document with none,
    /// because a client shows the errors of a broken document and the warnings of a working one.
    public static (IReadOnlyList<DocumentProblem> Errors, IReadOnlyList<DocumentProblem> Warnings) Check(DocumentValue document)
    {
        var errors = Unique(SchemaProblems(document).Concat(RuleProblems(document)));
        var warnings = errors.Count > 0 ? new List<DocumentProblem>() : Unique(WarningProblems((DocumentObject)document));
        return (errors, warnings);
    }

    /// Each problem once, in the order found: two rules can describe the same fault from two ends.
    private static List<DocumentProblem> Unique(IEnumerable<DocumentProblem> problems) => problems.Distinct().ToList();

    // MARK: small helpers, each the JavaScript reading the reference relies on

    private static string Join(string path, string key) => path.Length == 0 ? key : $"{path}.{key}";

    private static string? Text(DocumentValue? value) => value is DocumentString text ? text.Value : null;

    private static bool IsText(DocumentValue? value) => value is DocumentString { Value.Length: > 0 };

    private static bool IsInteger(DocumentValue? value, out double number)
    {
        number = value is DocumentNumber written ? written.AsDouble : double.NaN;
        return double.IsFinite(number) && Math.Floor(number) == number;
    }

    private static bool IsPortNumber(DocumentValue? value) => IsInteger(value, out var port) && port is >= 1 and <= 65535;

    private static IReadOnlyList<DocumentValue> ArrayOf(DocumentValue? value) =>
        value is DocumentArray array ? array.Items : Array.Empty<DocumentValue>();

    /// A value as a person reads it in a message.
    private static string Show(DocumentValue? value) => value is null ? "nothing" : DocumentJson.Print(value).TrimEnd('\n');

    /// JavaScript's `===` between two values read from a document: strings and numbers by value,
    /// literals by identity, and two objects or arrays never, because they are two objects.
    private static bool Same(DocumentValue? a, DocumentValue? b) => (a, b) switch
    {
        (null, null) => true,
        (DocumentString left, DocumentString right) => left.Value == right.Value,
        (DocumentNumber left, DocumentNumber right) => left.AsDouble == right.AsDouble,
        (DocumentLiteral left, DocumentLiteral right) => left.Value == right.Value,
        _ => false,
    };

    private static bool Matches(Regex grammar, string value, int maxCodePoints = int.MaxValue)
    {
        if (value.Contains('\n') || value.Contains('\r')) return false;
        var length = CodePoints(value);
        return length >= 1 && length <= maxCodePoints && grammar.IsMatch(value);
    }

    /// Code points, as a schema counts a string's length: a surrogate pair is one.
    private static int CodePoints(string value)
    {
        var count = 0;
        for (var index = 0; index < value.Length; index += 1)
        {
            if (char.IsHighSurrogate(value[index]) && index + 1 < value.Length && char.IsLowSurrogate(value[index + 1])) index += 1;
            count += 1;
        }
        return count;
    }

    private static bool IsToken(DocumentValue? value) => Text(value) is { } text && CommandSurface.IsValidToken(text);
    private static bool IsSetupId(DocumentValue? value) => Text(value) is { } text && CommandSurface.IsValidSetupId(text);
    private static bool IsConfiguredId(DocumentValue? value) => Text(value) is { } text && CommandSurface.IsValidConfiguredId(text);
    private static bool IsSha256(DocumentValue? value) => Text(value) is { } text && ControllerIdentity.IsHash(text);
    private static bool IsHostGuard(DocumentValue? value) => Text(value) is { } text && Matches(HostGuard, text, 253);
    private static bool IsUserGuard(DocumentValue? value) => Text(value) is { } text && Matches(UserGuard, text);
    private static bool IsArgItem(DocumentValue? value) => Text(value) is { } text && Matches(ArgItem, text, ArgItemBytes);
    private static bool IsRepo(DocumentValue? value) => Text(value) is { } text && LaunchValues.IsRepo(text);
    private static bool IsMac(DocumentValue? value) => Text(value) is { } text && Matches(MacAddress, text);
    private static bool OneOf(DocumentValue? value, string[] names) => Text(value) is { } text && names.Contains(text);

    // MARK: the schema, written out (code `schema`)

    private static List<DocumentProblem> SchemaProblems(DocumentValue document)
    {
        var problems = new List<DocumentProblem>();
        void Add(string path, string message) => problems.Add(new DocumentProblem("schema", path, message));

        // An optional string key held to [rule].
        void String(DocumentObject holder, string key, string path, Func<DocumentValue?, bool> rule, string what)
        {
            if (holder.Has(key) && !rule(holder[key])) Add(Join(path, key), $"{Join(path, key)} is {Show(holder[key])}, which is not {what}.");
        }
        // An optional array key: its type and limits, then [each] on every item.
        void List(DocumentObject holder, string key, string path, int min, int max, Action<DocumentValue, string> each)
        {
            if (!holder.Has(key)) return;
            var at = Join(path, key);
            if (holder[key] is not DocumentArray array)
            {
                Add(at, $"{at} is not a list.");
                return;
            }
            if (array.Items.Count < min) Add(at, $"{at} has {array.Items.Count} entries and needs at least {min}.");
            if (array.Items.Count > max) Add(at, $"{at} has {array.Items.Count} entries; at most {max} are allowed.");
            for (var index = 0; index < array.Items.Count; index += 1) each(array.Items[index], $"{at}[{index}]");
        }
        void Required(DocumentObject holder, string path, params string[] keys)
        {
            foreach (var key in keys)
                if (!holder.Has(key)) Add(Join(path, key), $"{Join(path, key)} is required.");
        }
        bool Object(DocumentValue? value, string path, string what)
        {
            if (value is DocumentObject) return true;
            Add(path, $"{(path.Length == 0 ? "the document" : path)} is not an object, as {what} has to be.");
            return false;
        }
        void Port(DocumentObject holder, string key, string path)
        {
            if (holder.Has(key) && !IsPortNumber(holder[key]))
                Add(Join(path, key), $"{Join(path, key)} is {Show(holder[key])}, which is not a port: an integer from 1 to 65535.");
        }
        void NonEmpty(DocumentValue value, string path, string what)
        {
            if (!IsText(value)) Add(path, $"{path} is {Show(value)}, which is not {what}.");
        }
        void Boolean(DocumentObject holder, string key, string path)
        {
            if (holder.Has(key) && holder[key] is not DocumentLiteral { Value: not null })
                Add(Join(path, key), $"{Join(path, key)} is {Show(holder[key])}, which is not true or false.");
        }

        if (!Object(document, "", "a setup document")) return problems;
        var root = (DocumentObject)document;

        if (!root.Has("version")) Add("version", "version is required, and it is 1.");
        else if (!(root["version"] is DocumentNumber version && version.AsDouble == 1))
            Add("version", $"version is {Show(root["version"])}; it stays 1.");
        if (!root.Has("machines")) Add("machines", "machines is required.");

        if (root.Has("controller") && Object(root["controller"], "controller", "the controller block"))
        {
            var block = (DocumentObject)root["controller"]!;
            Required(block, "controller", "id", "revision");
            String(block, "id", "controller", IsSetupId, "a setup id: a letter or digit, then up to 63 of letters, digits and . _ : -");
            String(block, "name", "controller", IsText, "a name");
            if (block.Has("revision") && !(IsInteger(block["revision"], out var revision) && revision >= 0))
                Add("controller.revision", $"controller.revision is {Show(block["revision"])}, which is not a revision: a whole number from 0.");
            if (block.Has("updatedAt") && block["updatedAt"] is not DocumentString)
                Add("controller.updatedAt", "controller.updatedAt is not a string.");
            if (block.Has("source") && !OneOf(block["source"], Sources))
                Add("controller.source", $"controller.source is {Show(block["source"])}, which is not one of {string.Join(", ", Sources)}; the device's own name goes in device.");
            String(block, "device", "controller", IsText, "a device name");
            List(block, "lineage", "controller", 0, ControllerIdentity.LineageLimit, (hash, path) =>
            {
                if (!IsSha256(hash)) Add(path, $"{path} is {Show(hash)}, which is not a lower-case sha256.");
            });
            if (block["lineage"] is DocumentArray lineage)
            {
                var seen = new HashSet<string>(StringComparer.Ordinal);
                for (var index = 0; index < lineage.Items.Count; index += 1)
                {
                    if (!seen.Add(Show(lineage.Items[index])))
                        Add($"controller.lineage[{index}]", $"{Show(lineage.Items[index])} is listed twice in the lineage; a document cannot descend from one ancestor twice.");
                }
            }
        }

        List(root, "sites", "", 0, 20, (site, path) =>
        {
            if (!Object(site, path, "a site")) return;
            var holder = (DocumentObject)site;
            Required(holder, path, "id");
            String(holder, "id", path, IsToken, "a plain token");
            String(holder, "name", path, IsText, "a name");
            List(holder, "lanPrefixes", path, 1, 10, (prefix, at) => NonEmpty(prefix, at, "a prefix"));
            List(holder, "broadcast", path, 0, 10, (address, at) => NonEmpty(address, at, "a broadcast address"));
        });

        void Helper(DocumentValue value, string path)
        {
            if (!Object(value, path, "a wake helper")) return;
            var holder = (DocumentObject)value;
            Required(holder, path, "machine", "action");
            String(holder, "machine", path, IsToken, "a plain token");
            String(holder, "action", path, IsConfiguredId, "an action id: a letter or digit, then letters, digits and . _ -");
        }

        List(root, "machines", "", 0, MaxMachines, (machine, path) =>
        {
            if (!Object(machine, path, "a machine")) return;
            var holder = (DocumentObject)machine;
            Required(holder, path, "id");
            String(holder, "id", path, IsToken, "a plain token");
            String(holder, "name", path, IsText, "a name");
            String(holder, "site", path, IsToken, "a plain token");
            Boolean(holder, "alwaysOn", path);

            List(holder, "endpoints", path, 0, 20, (endpoint, at) =>
            {
                if (!Object(endpoint, at, "an endpoint")) return;
                var address = (DocumentObject)endpoint;
                Required(address, at, "host");
                String(address, "id", at, IsToken, "a plain token");
                if (address.Has("kind") && !OneOf(address["kind"], Kinds))
                    Add(Join(at, "kind"), $"{Join(at, "kind")} is {Show(address["kind"])}, which is not lan or remote.");
                String(address, "host", at, IsHostGuard, "a host name or an address");
                Port(address, "port", at);
                String(address, "user", at, IsUserGuard, "a login name");
                String(address, "system", at, IsToken, "a plain token");
                String(address, "label", at, IsText, "a label");
            });

            if (holder.Has("wake") && Object(holder["wake"], Join(path, "wake"), "the wake block"))
            {
                var wake = (DocumentObject)holder["wake"]!;
                var at = Join(path, "wake");
                String(wake, "mac", at, IsMac, "a hardware address");
                List(wake, "broadcast", at, 0, 10, (address, where) => NonEmpty(address, where, "a broadcast address"));
                List(wake, "ports", at, 0, 5, (port, where) =>
                {
                    if (!IsPortNumber(port)) Add(where, $"{where} is {Show(port)}, which is not a port: an integer from 1 to 65535.");
                });
                if (wake.Has("probe") && Object(wake["probe"], Join(at, "probe"), "the wake probe"))
                {
                    var probe = (DocumentObject)wake["probe"]!;
                    Required(probe, Join(at, "probe"), "host");
                    String(probe, "host", Join(at, "probe"), IsHostGuard, "a host name or an address");
                    Port(probe, "port", Join(at, "probe"));
                }
                String(wake, "lanPrefix", at, IsText, "a prefix");
                if (wake.Has("helper")) Helper(wake["helper"]!, Join(at, "helper"));
                List(wake, "helpers", at, 0, 5, Helper);
            }

            List(holder, "systems", path, 0, 10, (system, at) =>
            {
                if (!Object(system, at, "a system")) return;
                var entry = (DocumentObject)system;
                Required(entry, at, "id");
                String(entry, "id", at, IsToken, "a plain token");
                String(entry, "name", at, IsText, "a name");
                if (entry.Has("platform") && !OneOf(entry["platform"], PlatformNames))
                    Add(Join(at, "platform"), $"{Join(at, "platform")} is {Show(entry["platform"])}, which is not one of {string.Join(", ", PlatformNames)}.");
                if (entry.Has("shell") && !OneOf(entry["shell"], ShellNames))
                    Add(Join(at, "shell"), $"{Join(at, "shell")} is {Show(entry["shell"])}, which is not one of {string.Join(", ", ShellNames)}.");
                Boolean(entry, "restricted", at);
                List(entry, "agent", at, 0, 20, (item, where) =>
                {
                    if (!IsArgItem(item))
                        Add(where, $"{where} is {Show(item)}, which is not an argv item: not empty, and no control character.");
                    else if (Encoding.UTF8.GetByteCount(Text(item)!) > ArgItemBytes)
                        Add(where, $"{where} is {Encoding.UTF8.GetByteCount(Text(item)!)} UTF-8 bytes, over the {ArgItemBytes} every dispatcher and client allows.");
                });
            });

            if (holder.Has("ssh") && Object(holder["ssh"], Join(path, "ssh"), "the ssh block"))
            {
                var ssh = (DocumentObject)holder["ssh"]!;
                var at = Join(path, "ssh");
                String(ssh, "host", at, IsHostGuard, "a host name or an address");
                String(ssh, "user", at, IsUserGuard, "a login name");
                Port(ssh, "port", at);
                String(ssh, "identityFile", at, IsText, "a path");
            }
        });

        if (root.Has("appUpdates") && Object(root["appUpdates"], "appUpdates", "the appUpdates block"))
        {
            var updates = (DocumentObject)root["appUpdates"]!;
            Required(updates, "appUpdates", "githubRepo");
            String(updates, "githubRepo", "appUpdates", IsRepo, "owner/name as GitHub spells them");
        }
        foreach (var key in new[] { "local", "ssh" })
        {
            if (root.Has(key) && root[key] is not (DocumentObject or DocumentLiteral { Value: null }))
                Add(key, $"{key} is neither an object nor null.");
        }
        return problems;
    }

    // MARK: the relationship rules (errors)

    /// Every wake helper of a machine: the entries of `wake.helpers` and the alias `wake.helper`.
    /// Both are checked, because clients disagree about which one counts when `helpers` is empty,
    /// and a helper some client would run must name a machine that exists and must not lead back
    /// to the machine it wakes.
    private static List<(string Machine, DocumentObject Helper)> HelpersOf(DocumentValue? machine)
    {
        var helpers = new List<(string, DocumentObject)>();
        if (machine is not DocumentObject holder || holder["wake"] is not DocumentObject wake) return helpers;
        var list = ArrayOf(wake["helpers"]).ToList();
        if (wake["helper"] is { } alias) list.Add(alias);
        foreach (var helper in list)
            if (helper is DocumentObject entry && Text(entry["machine"]) is { } id) helpers.Add((id, entry));
        return helpers;
    }

    private static string? IdOf(DocumentValue? value) => value is DocumentObject holder ? Text(holder["id"]) : null;

    private static List<DocumentProblem> RuleProblems(DocumentValue document)
    {
        var problems = new List<DocumentProblem>();
        void Add(string code, string path, string message) => problems.Add(new DocumentProblem(code, path, message));
        if (document is not DocumentObject root) return problems;
        var machines = ArrayOf(root["machines"]);
        var sites = ArrayOf(root["sites"]);

        var machineIds = new HashSet<string>(StringComparer.Ordinal);
        for (var index = 0; index < machines.Count; index += 1)
        {
            if (IdOf(machines[index]) is not { } id) continue;
            var machine = (DocumentObject)machines[index];
            if (!machineIds.Add(id)) Add("duplicate-machine", $"machines[{index}].id", $"Two machines share the id \"{id}\".");

            // Endpoints without an id are never duplicates: they cannot be remembered as a route,
            // and that is a warning, not a reason to refuse the setup.
            var endpointIds = new HashSet<string>(StringComparer.Ordinal);
            var endpoints = ArrayOf(machine["endpoints"]);
            for (var k = 0; k < endpoints.Count; k += 1)
            {
                if (IdOf(endpoints[k]) is { } endpoint && !endpointIds.Add(endpoint))
                    Add("duplicate-endpoint", $"machines[{index}].endpoints[{k}].id",
                        $"{id}: two endpoints share the id \"{endpoint}\", so a remembered route is ambiguous.");
            }

            var systemIds = new HashSet<string>(StringComparer.Ordinal);
            var systems = ArrayOf(machine["systems"]);
            for (var k = 0; k < systems.Count; k += 1)
            {
                if (IdOf(systems[k]) is { } system && !systemIds.Add(system))
                    Add("duplicate-system", $"machines[{index}].systems[{k}].id", $"{id}: two systems share the id \"{system}\".");
            }
        }

        var siteIds = new HashSet<string>(StringComparer.Ordinal);
        for (var index = 0; index < sites.Count; index += 1)
        {
            if (IdOf(sites[index]) is { } id && !siteIds.Add(id)) Add("duplicate-site", $"sites[{index}].id", $"Two sites share the id \"{id}\".");
        }

        for (var index = 0; index < machines.Count; index += 1)
        {
            if (machines[index] is not DocumentObject machine) continue;
            var id = Text(machine["id"]);
            var name = id ?? $"machines[{index}]";
            // A setup without sites places nobody, so a site name is only checked against a list
            // that exists.
            if (Text(machine["site"]) is { } site && sites.Count > 0 && !siteIds.Contains(site))
                Add("unknown-site", $"machines[{index}].site", $"{name}: the site \"{site}\" is not in sites, so this machine drops out of every on-site decision.");
            foreach (var (helper, _) in HelpersOf(machine))
            {
                if (helper == id)
                    Add("helper-self", $"machines[{index}].wake", $"{name} is its own wake helper, which asks a machine that is off to wake itself.");
                else if (!machineIds.Contains(helper))
                    Add("helper-unknown", $"machines[{index}].wake", $"{name}: the wake helper \"{helper}\" is not a machine in this setup.");
            }
            // The singular key is what a 1.2 client reads. If the two disagree, an old and a new
            // client try different machines from the same document.
            if (machine["wake"] is DocumentObject wake && wake["helper"] is DocumentObject alias
                && wake["helpers"] is DocumentArray { Items: [DocumentObject first, ..] }
                && (!Same(first["machine"], alias["machine"]) || !Same(first["action"], alias["action"])))
            {
                Add("helper-alias", $"machines[{index}].wake.helper",
                    $"{name}: wake.helper names {Show(alias["machine"])}/{Show(alias["action"])} and wake.helpers starts with {Show(first["machine"])}/{Show(first["action"])}. The singular key repeats the first entry for older clients, so the two have to agree.");
            }
        }

        // A ring of helpers is the failure that looks fine one machine at a time: every machine in
        // it waits for another to be awake first. The walk costs the square of the machines, so a
        // document with more of them than the schema allows, which is refused already, is not
        // walked: a peer could otherwise hand this device a megabyte of machines to chase.
        if (machines.Count > MaxMachines) return problems;
        var byId = new Dictionary<string, DocumentObject>(StringComparer.Ordinal);
        foreach (var machine in machines)
            if (IdOf(machine) is { } id) byId.TryAdd(id, (DocumentObject)machine);
        for (var index = 0; index < machines.Count; index += 1)
        {
            if (IdOf(machines[index]) is not { } origin) continue;
            var seen = new HashSet<string>(StringComparer.Ordinal) { origin };
            void Walk(string id, List<string> trail)
            {
                foreach (var (helper, _) in HelpersOf(byId.GetValueOrDefault(id)))
                {
                    if (seen.Contains(helper))
                    {
                        if (helper == origin && trail.Count > 0)
                            Add("helper-cycle", $"machines[{index}].wake",
                                $"The wake helpers form a cycle: {string.Join(" needs ", trail.Append(id).Append(helper))}. Every machine in it waits for another to be awake first.");
                        continue;
                    }
                    if (!byId.ContainsKey(helper)) continue;
                    seen.Add(helper);
                    Walk(helper, trail.Append(id).ToList());
                }
            }
            Walk(origin, new List<string>());
        }
        return problems;
    }

    // MARK: what this app cannot use (warnings), only read for a document with no error, so every
    // shape below is one the schema allows

    private static List<DocumentProblem> WarningProblems(DocumentObject document)
    {
        var warnings = new List<DocumentProblem>();
        void Add(string code, string path, string message) => warnings.Add(new DocumentProblem(code, path, message));
        var machines = ArrayOf(document["machines"]).Cast<DocumentObject>().ToList();
        var siteIds = ArrayOf(document["sites"]).Select(IdOf).ToHashSet(StringComparer.Ordinal);
        var byId = new Dictionary<string, DocumentObject>(StringComparer.Ordinal);
        foreach (var machine in machines) byId.TryAdd(IdOf(machine)!, machine);

        if (machines.Count == 0) Add("no-machines", "machines", "The setup has no machines yet, so there is nothing to drive.");

        // The dial grammar of section 15.6, on a value the guard already passed.
        void Dialable(DocumentObject holder, string path, string owner)
        {
            if (Text(holder["host"]) is { } host && Addresses.HostForm(host) is null)
                Add("endpoint-undialable", Join(path, "host"), $"{owner}: \"{host}\" passes the setup's host guard but is not a name or an address this app dials, scans or pins, so it is skipped.");
            if (Text(holder["user"]) is { } user && !Addresses.IsUser(user))
                Add("endpoint-undialable", Join(path, "user"), $"{owner}: \"{user}\" is not a login name this app dials with, so it is skipped.");
            if (holder.Has("port") && !IsPortNumber(holder["port"]))
                Add("endpoint-undialable", Join(path, "port"), $"{owner}: {Show(holder["port"])} is not a port this app dials, so it is skipped.");
        }

        for (var index = 0; index < machines.Count; index += 1)
        {
            var machine = machines[index];
            var id = IdOf(machine)!;
            var path = $"machines[{index}]";
            var systems = ArrayOf(machine["systems"]).Cast<DocumentObject>().ToList();
            var endpoints = ArrayOf(machine["endpoints"]).Cast<DocumentObject>().ToList();
            var systemIds = systems.Select(IdOf).ToHashSet(StringComparer.Ordinal);
            var legacyHost = machine["ssh"] is DocumentObject legacy && legacy["host"] is DocumentString;

            if (systems.Count == 0) Add("machine-no-systems", path, $"Machine \"{id}\" lists no systems, so nothing on it can be driven yet.");
            if (endpoints.Count == 0 && !legacyHost) Add("machine-no-endpoints", path, $"Machine \"{id}\" has no address, so there is no way to reach it yet.");

            for (var k = 0; k < systems.Count; k += 1)
            {
                if (ArrayOf(systems[k]["agent"]).Count == 0)
                    Add("system-no-agent", $"{path}.systems[{k}]", $"System \"{IdOf(systems[k])}\" on machine \"{id}\" has no agent command, so it is not driven.");
            }

            for (var k = 0; k < endpoints.Count; k += 1)
            {
                var endpoint = endpoints[k];
                var at = $"{path}.endpoints[{k}]";
                if (!endpoint.Has("id"))
                    Add("endpoint-no-id", at, $"{id}: an address has no id, so it cannot be remembered as a route, and it merges with the other addresses that have none.");
                if (!endpoint.Has("user"))
                    Add("endpoint-no-user", at, $"{id}: an address has no user, so ssh logs in as whatever this device's own ssh configuration says.");
                if (Text(endpoint["system"]) is { } system && !systemIds.Contains(system))
                    Add("endpoint-unknown-system", Join(at, "system"), $"{id}: an address names the system \"{system}\", which this machine does not have, so the hint is ignored.");
                Dialable(endpoint, at, id);
            }
            if (machine["ssh"] is DocumentObject ssh) Dialable(ssh, $"{path}.ssh", id);

            if (machine["wake"] is not DocumentObject wake) continue;
            var helpers = HelpersOf(machine);
            if (!wake.Has("mac"))
                Add("wake-no-mac", $"{path}.wake", $"{id}: the wake block has no hardware address, so no packet can wake it directly. Helpers still work.");
            var namesSite = Text(machine["site"]) is { } site && siteIds.Contains(site);
            if (helpers.Count == 0 && !namesSite && LanPrefix.Parse(Text(wake["lanPrefix"])) is null)
                Add("wake-unplaced", $"{path}.wake", $"Machine \"{id}\" names no site, no valid lanPrefix and no helper, so no device can tell when it is on its network and nothing can wake it.");
            if (wake["helper"] is DocumentObject && wake["helpers"] is DocumentArray { Items.Count: 0 })
                Add("helper-alias-unused", $"{path}.wake.helper", $"{id}: wake.helper is set beside an empty wake.helpers, which some clients read as no helper at all and others as this one. Write it into helpers as well.");
            foreach (var (helper, _) in helpers)
            {
                if (!byId.TryGetValue(helper, out var target)) continue;
                if (Text(machine["site"]) is { } here && Text(target["site"]) is { } there && there != here)
                    Add("helper-other-site", $"{path}.wake", $"The helper \"{helper}\" for \"{id}\" is at another site. That only works if its action reaches the target's network another way, through a router or a VPN.");
                if (target["alwaysOn"] is not DocumentLiteral { Value: true })
                    Add("helper-not-always-on", $"{path}.wake", $"The helper \"{helper}\" for \"{id}\" is not marked as always on, so it may be asleep when it is needed.");
            }
        }

        warnings.AddRange(PrefixWarnings(document));
        return warnings;
    }

    /// The warnings that come from addresses: `shared-prefix`, `prefix-invalid`,
    /// `prefix-without-dot`, `broadcast-invalid` and `wake-broadcast-defaulted`, in document order.
    /// None refuses a document; each says something a person should know about how the setup will
    /// behave.
    private static List<DocumentProblem> PrefixWarnings(DocumentObject document)
    {
        var warnings = new List<DocumentProblem>();
        void Add(string code, string path, string message) => warnings.Add(new DocumentProblem(code, path, message));
        var sites = ArrayOf(document["sites"]);
        var machines = ArrayOf(document["machines"]);

        LanPrefix? CheckPrefix(string text, string path)
        {
            var parsed = LanPrefix.Parse(text);
            if (parsed is null)
                Add("prefix-invalid", path, $"\"{text}\" is not a dotted or CIDR IPv4 prefix, so it never matches any address.");
            else if (parsed.Value.Dotted && !text.EndsWith('.'))
                Add("prefix-without-dot", path, WithoutDot(text));
            return parsed;
        }
        void CheckBroadcasts(DocumentValue? list, string path)
        {
            var entries = ArrayOf(list);
            for (var index = 0; index < entries.Count; index += 1)
            {
                if (LanPrefix.ParseIPv4(Text(entries[index])) is null)
                    Add("broadcast-invalid", $"{path}[{index}]", $"{Show(entries[index])} is not a dotted IPv4 address, so no client sends a packet to it and none resolves it as a name.");
            }
        }

        // Each site's prefixes and broadcasts, remembering the valid prefixes for the overlap check.
        var parsedBySite = new List<List<(string Text, LanPrefix Prefix)>>();
        for (var index = 0; index < sites.Count; index += 1)
        {
            var parsed = new List<(string, LanPrefix)>();
            parsedBySite.Add(parsed);
            if (sites[index] is not DocumentObject site) continue;
            var prefixes = ArrayOf(site["lanPrefixes"]);
            for (var k = 0; k < prefixes.Count; k += 1)
            {
                if (Text(prefixes[k]) is not { } text) continue;
                if (CheckPrefix(text, $"sites[{index}].lanPrefixes[{k}]") is { } prefix) parsed.Add((text, prefix));
            }
            CheckBroadcasts(site["broadcast"], $"sites[{index}].broadcast");
        }

        // Overlapping prefixes on two sites are legal and expected: home routers ship the same
        // default range. The warning says that matching one no longer tells which site a device is
        // on, and that its location is then unconfirmed unless a person chose one.
        for (var j = 1; j < sites.Count; j += 1)
        {
            for (var i = 0; i < j; i += 1)
            {
                var pair = parsedBySite[i].SelectMany(a => parsedBySite[j].Select(b => (A: a, B: b)))
                    .FirstOrDefault(pair => pair.A.Prefix.Overlaps(pair.B.Prefix));
                if (pair.A.Text is null) continue;
                var what = pair.A.Text == pair.B.Text
                    ? $"both use the prefix {pair.A.Text}"
                    : $"have the overlapping prefixes {pair.A.Text} and {pair.B.Text}";
                Add("shared-prefix", $"sites[{j}].lanPrefixes",
                    $"The sites {IdOf(sites[i])} and {IdOf(sites[j])} {what}, so matching it does not say which site this device is on. Pick one in this device's own settings to be sure.");
            }
        }

        var siteById = new Dictionary<string, DocumentObject>(StringComparer.Ordinal);
        foreach (var site in sites)
            if (IdOf(site) is { } id) siteById.TryAdd(id, (DocumentObject)site);

        for (var index = 0; index < machines.Count; index += 1)
        {
            if (machines[index] is not DocumentObject machine || machine["wake"] is not DocumentObject wake) continue;
            var path = $"machines[{index}].wake";
            var site = Text(machine["site"]) is { } siteId ? siteById.GetValueOrDefault(siteId) : null;
            var lanPrefix = wake.Has("lanPrefix") && Text(wake["lanPrefix"]) is { } prefixText ? CheckPrefix(prefixText, $"{path}.lanPrefix") : null;
            CheckBroadcasts(wake["broadcast"], $"{path}.broadcast");

            // A machine that can be woken directly: it has a hardware address and a place, a site
            // in sites or, only when it names none, a valid lanPrefix. With no dotted broadcast at
            // either level, direct wake falls back to the derived or the limited broadcast.
            var placed = site is not null || lanPrefix is not null;
            if (wake["mac"] is not DocumentString || !placed) continue;
            if (!ArrayOf(wake["broadcast"]).Any(entry => LanPrefix.ParseIPv4(Text(entry)) is not null)
                && (site is null || !ArrayOf(site["broadcast"]).Any(entry => LanPrefix.ParseIPv4(Text(entry)) is not null)))
            {
                Add("wake-broadcast-defaulted", path,
                    $"{IdOf(machine)} can be woken directly and names no dotted broadcast address{(site is null ? "" : $", and neither does the site {IdOf(site)}")}, so a packet goes to the broadcast this device works out from its own address, or to 255.255.255.255.");
            }
        }
        return warnings;
    }

    /// What a client that predates section 10.1 does with a dotted prefix written without its
    /// trailing dot: it matches the prefix as text, so it also takes an address whose next octet
    /// merely begins with the prefix's last one.
    private static string WithoutDot(string text)
    {
        var last = int.TryParse(text[(text.LastIndexOf('.') + 1)..], NumberStyles.None, CultureInfo.InvariantCulture, out var octet) ? octet : -1;
        var start = $"\"{text}\" has no trailing dot, and clients that predate section 10.1 match it as text";
        return last is >= 1 and <= 25
            ? $"{start}, so they also take {text}0.x for {text}.x."
            : $"{start}; it takes nothing more here only because no octet begins with {last} but {last} itself.";
    }
}
