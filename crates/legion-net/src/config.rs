//! `fleet.toml`: the machines this node can reach, validated strictly and edited in place with
//! `toml_edit` so comments and unrelated entries survive tool edits.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::Mutex;

use legion_core::config::{Document, NodeName, parse};
use legion_os::atomic::{WriteOptions, write_atomic};
use legion_proto::{ErrorBody, ErrorCode};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value as TomlValue};

/// The implicit system name reported when a machine uses the shorthand single-system form.
pub const DEFAULT_SYSTEM_NAME: &str = "default";

/// Serializes in-process `fleet.toml` mutations so concurrent admin calls cannot race each other.
static FLEET_EDIT_LOCK: Mutex<()> = Mutex::new(());

/// Operating system family hint used to choose the remote ssh command shape without probing first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RemoteOs {
    /// Linux, macOS, or another POSIX-like host reached through `sh -c`.
    Unix,
    /// Windows host where `legion` is on `PATH`.
    Windows,
}

impl RemoteOs {
    /// The other operating system family, tried as a fallback when no hint is configured.
    pub fn other(self) -> RemoteOs {
        match self {
            RemoteOs::Unix => RemoteOs::Windows,
            RemoteOs::Windows => RemoteOs::Unix,
        }
    }

    /// The lower-case spelling written in `fleet.toml` and state files.
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteOs::Unix => "unix",
            RemoteOs::Windows => "windows",
        }
    }
}

/// `fleet.toml`: every machine this node knows how to reach.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetFile {
    /// Named machines, keyed by machine name.
    #[serde(default)]
    pub machines: BTreeMap<String, MachineEntry>,
}

impl FleetFile {
    /// Looks up a configured machine by name.
    pub fn machine(&self, name: &str) -> Option<&MachineEntry> {
        self.machines.get(name)
    }

    /// Sorted names of every machine in the file.
    pub fn names(&self) -> Vec<&str> {
        self.machines.keys().map(String::as_str).collect()
    }

    /// Whether any machine or system already targets this ssh host string.
    pub fn has_ssh_target(&self, target: &str) -> bool {
        self.machines.values().any(|machine| {
            machine.resolved_systems().into_iter().any(|sys| {
                matches!(sys.transport, Transport::Ssh { target: ref existing, .. } if existing == target)
            })
        })
    }
}

impl Document for FleetFile {
    const FILE: &'static str = "fleet.toml";

    fn validate(&self) -> Result<(), String> {
        for (name, entry) in &self.machines {
            validate_machine_entry(name, entry)?;
        }
        Ok(())
    }
}

/// One `[machines.<name>]` table in `fleet.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineEntry {
    /// Shorthand SSH target for a single-system machine.
    #[serde(default)]
    pub ssh: Option<String>,
    /// Shorthand exact argv for a single-system machine reached through a custom command.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Remote `legion` binary name or path for the shorthand SSH form.
    #[serde(default)]
    pub program: Option<String>,
    /// Remote operating system hint for the shorthand SSH form.
    #[serde(default)]
    pub os: Option<RemoteOs>,
    /// Multiple bootable systems for a dual-boot or multi-system machine.
    #[serde(default)]
    pub systems: Option<Vec<SystemEntry>>,
    /// Hardware MAC address for Wake-on-LAN.
    #[serde(default)]
    pub mac: Option<String>,
    /// Helper machines that can wake this machine.
    #[serde(default)]
    pub wake_via: Vec<String>,
}

impl MachineEntry {
    /// Returns every system of this machine in uniform form, whether configured via `[[systems]]`
    /// or the single-system shorthand.
    pub fn resolved_systems(&self) -> Vec<ResolvedSystem> {
        if let Some(systems) = &self.systems {
            systems
                .iter()
                .filter_map(|sys| {
                    let transport = transport_from_fields(
                        sys.ssh.as_ref(),
                        sys.command.as_ref(),
                        sys.program.as_ref(),
                        sys.os,
                    )?;
                    Some(ResolvedSystem {
                        name: sys.name.clone(),
                        explicit: true,
                        transport,
                    })
                })
                .collect()
        } else if let Some(transport) = transport_from_fields(
            self.ssh.as_ref(),
            self.command.as_ref(),
            self.program.as_ref(),
            self.os,
        ) {
            vec![ResolvedSystem {
                name: DEFAULT_SYSTEM_NAME.to_owned(),
                explicit: false,
                transport,
            }]
        } else {
            Vec::new()
        }
    }
}

/// One `[[machines.<name>.systems]]` entry in `fleet.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemEntry {
    /// Name of the bootable system, such as `linux` or `windows`.
    pub name: String,
    /// SSH target accepted by the system ssh client.
    #[serde(default)]
    pub ssh: Option<String>,
    /// Exact argv to start `legion rpc`.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Remote `legion` binary name or path when it is not found the default way.
    #[serde(default)]
    pub program: Option<String>,
    /// Operating system hint (`"unix"` or `"windows"`).
    #[serde(default)]
    pub os: Option<RemoteOs>,
}

/// How a single system of a machine is reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Reached through the system `ssh` client.
    Ssh {
        /// SSH destination string passed as one argument to `ssh`.
        target: String,
        /// Optional remote `legion` program override.
        program: Option<String>,
        /// Optional remote OS hint.
        os: Option<RemoteOs>,
    },
    /// Reached by running an exact argv locally.
    Command {
        /// Exact program and arguments.
        argv: Vec<String>,
    },
}

/// One normalized system belonging to a machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSystem {
    /// System name (`"default"` when the machine used single-system shorthand).
    pub name: String,
    /// `true` when configured under `[[machines.<name>.systems]]`.
    pub explicit: bool,
    /// Connector transport for this system.
    pub transport: Transport,
}

fn transport_from_fields(
    ssh: Option<&String>,
    command: Option<&Vec<String>>,
    program: Option<&String>,
    os: Option<RemoteOs>,
) -> Option<Transport> {
    match (ssh, command) {
        (Some(target), None) => Some(Transport::Ssh {
            target: target.clone(),
            program: program.cloned(),
            os,
        }),
        (None, Some(argv)) => Some(Transport::Command { argv: argv.clone() }),
        _ => None,
    }
}

/// Checks that `name` obeys the same rules as a Legion node name.
pub fn validate_machine_name(name: &str) -> Result<(), String> {
    NodeName::try_from(name.to_owned()).map(|_| ())
}

/// Checks that an SSH destination string cannot be mistaken for an option or carry control bytes.
pub fn validate_ssh_target(target: &str) -> Result<(), String> {
    if target.is_empty() {
        return Err("ssh target cannot be empty".to_owned());
    }
    if target.starts_with('-') {
        return Err(format!(
            "ssh target {target:?} cannot begin with `-`, which ssh would read as an option"
        ));
    }
    if target.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "ssh target {target:?} cannot contain spaces or control characters"
        ));
    }
    Ok(())
}

/// Checks that a configured remote `program` needs no shell quoting on any supported shell.
///
/// Only ASCII letters, digits, `_ . / ~ : - \` are allowed. Leading `-` is refused so the name
/// cannot be parsed as an option by `command -v` or `exec`, and a trailing `\` is refused because
/// `fish` treats `\'` inside single quotes as an escaped quote.
pub fn validate_program(program: &str) -> Result<(), String> {
    if program.is_empty() {
        return Err("program cannot be empty".to_owned());
    }
    if program.starts_with('-') {
        return Err(format!(
            "program {program:?} cannot begin with `-`, which a shell builtin would read as an option"
        ));
    }
    if program.ends_with('\\') {
        return Err(format!("program {program:?} cannot end with `\\`"));
    }
    if let Some(bad) = program.chars().find(|&c| !is_safe_program_char(c)) {
        return Err(format!(
            "program {program:?} contains {bad:?}; only A-Z, a-z, 0-9, `_`, `.`, `/`, `~`, `:`, `-`, and `\\` are allowed so nothing is ever quoted for a remote shell"
        ));
    }
    Ok(())
}

fn is_safe_program_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '~' | ':' | '-' | '\\')
}

/// Checks a hardware MAC address (`02:00:00:00:00:01` or `02-00-00-00-00-01`).
pub fn validate_mac(mac: &str) -> Result<(), String> {
    let bytes = mac.as_bytes();
    if bytes.len() != 17 {
        return Err(format!(
            "{mac:?} is not a MAC address: write six hex pairs such as 02:00:00:00:00:01"
        ));
    }
    let sep = bytes[2];
    if sep != b':' && sep != b'-' {
        return Err(format!(
            "{mac:?} is not a MAC address: separate six hex pairs with `:` or `-`"
        ));
    }
    for (idx, &b) in bytes.iter().enumerate() {
        if idx % 3 == 2 {
            if b != sep {
                return Err(format!(
                    "{mac:?} is not a MAC address: use one consistent separator (`:` or `-`)"
                ));
            }
        } else if !b.is_ascii_hexdigit() {
            return Err(format!(
                "{mac:?} is not a MAC address: every octet must be two hexadecimal digits"
            ));
        }
    }
    Ok(())
}

/// Validates one machine entry, naming both the machine and the offending field in every error.
pub fn validate_machine_entry(name: &str, entry: &MachineEntry) -> Result<(), String> {
    validate_machine_name(name).map_err(|reason| format!("machines.{name} (name): {reason}"))?;

    if let Some(mac) = &entry.mac {
        validate_mac(mac).map_err(|reason| format!("machines.{name}.mac: {reason}"))?;
    }

    let mut seen_helpers = HashSet::new();
    for (index, helper) in entry.wake_via.iter().enumerate() {
        validate_machine_name(helper)
            .map_err(|reason| format!("machines.{name}.wake_via[{index}]: {reason}"))?;
        if helper == name {
            return Err(format!(
                "machines.{name}.wake_via: a machine cannot list itself as a wake helper"
            ));
        }
        if !seen_helpers.insert(helper.as_str()) {
            return Err(format!(
                "machines.{name}.wake_via: {helper:?} is listed more than once"
            ));
        }
    }

    if let Some(systems) = &entry.systems {
        if entry.ssh.is_some() {
            return Err(format!(
                "machines.{name}.ssh cannot be combined with systems; set ssh on each system entry"
            ));
        }
        if entry.command.is_some() {
            return Err(format!(
                "machines.{name}.command cannot be combined with systems; set command on each system entry"
            ));
        }
        if entry.program.is_some() {
            return Err(format!(
                "machines.{name}.program cannot be combined with systems; set program on each system entry"
            ));
        }
        if entry.os.is_some() {
            return Err(format!(
                "machines.{name}.os cannot be combined with systems; set os on each system entry"
            ));
        }
        if systems.is_empty() {
            return Err(format!(
                "machines.{name}.systems cannot be empty; add at least one system or use ssh/command"
            ));
        }
        let mut seen_systems = HashSet::new();
        for (index, sys) in systems.iter().enumerate() {
            validate_machine_name(&sys.name)
                .map_err(|reason| format!("machines.{name}.systems[{index}].name: {reason}"))?;
            if !seen_systems.insert(sys.name.as_str()) {
                return Err(format!(
                    "machines.{name}.systems: duplicate system name {:?}",
                    sys.name
                ));
            }
            let prefix = format!("machines.{name}.systems.{}", sys.name);
            validate_transport(
                &prefix,
                sys.ssh.as_deref(),
                sys.command.as_deref(),
                sys.program.as_deref(),
            )?;
        }
    } else {
        let prefix = format!("machines.{name}");
        if entry.ssh.is_none() && entry.command.is_none() {
            return Err(format!("{prefix}: set one of ssh, command, or systems"));
        }
        validate_transport(
            &prefix,
            entry.ssh.as_deref(),
            entry.command.as_deref(),
            entry.program.as_deref(),
        )?;
    }

    Ok(())
}

fn validate_transport(
    prefix: &str,
    ssh: Option<&str>,
    command: Option<&[String]>,
    program: Option<&str>,
) -> Result<(), String> {
    match (ssh, command) {
        (Some(_), Some(_)) => {
            return Err(format!("{prefix}: set either ssh or command, not both"));
        }
        (None, None) => {
            return Err(format!("{prefix}: set either ssh or command"));
        }
        (Some(target), None) => {
            validate_ssh_target(target).map_err(|reason| format!("{prefix}.ssh: {reason}"))?;
            if let Some(prog) = program {
                validate_program(prog).map_err(|reason| format!("{prefix}.program: {reason}"))?;
            }
        }
        (None, Some(argv)) => {
            if argv.is_empty() || argv[0].is_empty() {
                return Err(format!(
                    "{prefix}.command: command must contain at least a non-empty program name"
                ));
            }
            if program.is_some() {
                return Err(format!(
                    "{prefix}.program cannot be combined with command; command already specifies the full argv"
                ));
            }
        }
    }
    Ok(())
}

/// Atomically adds or replaces one machine in `fleet.toml`, preserving comments and unrelated
/// entries, and returns `true` if an existing entry was replaced.
pub fn upsert_machine(path: &Path, name: &str, entry: &MachineEntry) -> Result<bool, ErrorBody> {
    validate_machine_entry(name, entry).map_err(|reason| {
        ErrorBody::new(ErrorCode::InvalidArgs, format!("{reason}."))
            .with_hint("Fix the machine settings and try again.")
    })?;
    let _guard = FLEET_EDIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut doc = load_editable(path)?;
    let machines = ensure_machines_table(&mut doc, path)?;
    let replaced = apply_upsert(machines, name, entry);
    save_validated(path, &doc)?;
    Ok(replaced)
}

/// Atomically adds multiple new machines to `fleet.toml` in a single edit, skipping any already
/// present and returning the names that were added.
pub fn add_missing_machines(
    path: &Path,
    entries: &[(String, MachineEntry)],
) -> Result<Vec<String>, ErrorBody> {
    for (name, entry) in entries {
        validate_machine_entry(name, entry)
            .map_err(|reason| ErrorBody::new(ErrorCode::InvalidArgs, format!("{reason}.")))?;
    }
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let _guard = FLEET_EDIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut doc = load_editable(path)?;
    let machines = ensure_machines_table(&mut doc, path)?;
    let mut added = Vec::new();
    for (name, entry) in entries {
        if !machines.contains_key(name) {
            apply_upsert(machines, name, entry);
            added.push(name.clone());
        }
    }
    if !added.is_empty() {
        save_validated(path, &doc)?;
    }
    Ok(added)
}

/// Atomically removes one machine from `fleet.toml`, preserving comments and unrelated entries.
pub fn remove_machine(path: &Path, name: &str) -> Result<(), ErrorBody> {
    validate_machine_name(name)
        .map_err(|reason| ErrorBody::new(ErrorCode::InvalidArgs, format!("{reason}.")))?;
    let _guard = FLEET_EDIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut doc = load_editable(path)?;
    let known: Vec<String> = doc
        .get("machines")
        .and_then(Item::as_table)
        .map(|table| table.iter().map(|(k, _)| k.to_owned()).collect())
        .unwrap_or_default();
    let Some(machines) = doc.get_mut("machines").and_then(Item::as_table_mut) else {
        return Err(not_found_in_file(name, &known));
    };
    if machines.remove(name).is_none() {
        return Err(not_found_in_file(name, &known));
    }
    save_validated(path, &doc)?;
    Ok(())
}

fn not_found_in_file(name: &str, known: &[String]) -> ErrorBody {
    let hint = if known.is_empty() {
        "fleet.toml has no machines yet.".to_owned()
    } else {
        format!("Known machines in fleet.toml: {}.", known.join(", "))
    };
    ErrorBody::new(
        ErrorCode::NotFound,
        format!("There is no machine called {name} in fleet.toml."),
    )
    .with_hint(hint)
}

fn load_editable(path: &Path) -> Result<DocumentMut, ErrorBody> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(DocumentMut::new()),
        Err(err) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {err}.", path.display()),
            )
            .with_hint("Make fleet.toml readable by the account legion runs as."));
        }
    };
    // Refuse to edit a file that does not already parse as a valid FleetFile.
    parse::<FleetFile>(path, text.as_bytes())?;
    text.parse::<DocumentMut>().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be edited: {err}.", path.display()),
        )
        .with_hint("Fix the syntax error in fleet.toml before editing it with fleet tools.")
    })
}

fn ensure_machines_table<'a>(
    doc: &'a mut DocumentMut,
    path: &Path,
) -> Result<&'a mut Table, ErrorBody> {
    if !doc.contains_key("machines") {
        let mut machines = Table::new();
        machines.set_implicit(true);
        doc.insert("machines", Item::Table(machines));
    }
    doc.get_mut("machines")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} has a non-table `machines` key.", path.display()),
            )
            .with_hint("Ensure `[machines]` is a TOML table in fleet.toml.")
        })
}

fn apply_upsert(machines: &mut Table, name: &str, entry: &MachineEntry) -> bool {
    let existing_decor = machines
        .get(name)
        .and_then(Item::as_table)
        .map(|t| t.decor().clone());
    let replaced = machines.contains_key(name);
    let mut table = build_machine_table(entry);
    if let Some(decor) = existing_decor {
        *table.decor_mut() = decor;
    }
    machines.insert(name, Item::Table(table));
    replaced
}

fn build_machine_table(entry: &MachineEntry) -> Table {
    let mut table = Table::new();
    if let Some(ssh) = &entry.ssh {
        table.insert("ssh", toml_edit::value(ssh.as_str()));
    }
    if let Some(command) = &entry.command {
        table.insert(
            "command",
            Item::Value(TomlValue::Array(string_array(command))),
        );
    }
    if let Some(program) = &entry.program {
        table.insert("program", toml_edit::value(program.as_str()));
    }
    if let Some(os) = entry.os {
        table.insert("os", toml_edit::value(os.as_str()));
    }
    if let Some(mac) = &entry.mac {
        table.insert("mac", toml_edit::value(mac.as_str()));
    }
    if !entry.wake_via.is_empty() {
        table.insert(
            "wake_via",
            Item::Value(TomlValue::Array(string_array(&entry.wake_via))),
        );
    }
    if let Some(systems) = &entry.systems {
        let mut array_of_tables = ArrayOfTables::new();
        for sys in systems {
            let mut st = Table::new();
            st.insert("name", toml_edit::value(sys.name.as_str()));
            if let Some(ssh) = &sys.ssh {
                st.insert("ssh", toml_edit::value(ssh.as_str()));
            }
            if let Some(command) = &sys.command {
                st.insert(
                    "command",
                    Item::Value(TomlValue::Array(string_array(command))),
                );
            }
            if let Some(program) = &sys.program {
                st.insert("program", toml_edit::value(program.as_str()));
            }
            if let Some(os) = sys.os {
                st.insert("os", toml_edit::value(os.as_str()));
            }
            array_of_tables.push(st);
        }
        table.insert("systems", Item::ArrayOfTables(array_of_tables));
    }
    table
}

fn string_array(items: &[String]) -> Array {
    let mut array = Array::new();
    for item in items {
        array.push(item.as_str());
    }
    array
}

fn save_validated(path: &Path, doc: &DocumentMut) -> Result<(), ErrorBody> {
    let mut rendered = doc.to_string();
    if !rendered.is_empty() && !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    // Validate the exact bytes about to be written through legion-core's strict loader.
    parse::<FleetFile>(path, rendered.as_bytes())?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {err}.", parent.display()),
            )
            .with_hint(
                "Give the account legion runs as write access to the configuration directory.",
            )
        })?;
    }
    write_atomic(path, rendered.as_bytes(), WriteOptions::default()).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be written: {err}.", path.display()),
        )
        .with_hint("Give the account legion runs as write access to the configuration directory.")
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use legion_core::config::parse;

    use super::*;

    const DESIGN_EXAMPLE: &str = r#"
# Fleet configuration
[machines.tower]
mac = "02:00:00:00:00:01"
wake_via = ["pi"]

[[machines.tower.systems]]
name = "linux"
ssh = "tower"
os = "unix"

[[machines.tower.systems]]
name = "windows"
ssh = "tower-win"
os = "windows"
program = 'C:\Tools\legion.exe'

[machines.pi]
ssh = "pi"

[machines.box]
command = ["docker", "exec", "-i", "box", "legion", "rpc"]
"#;

    #[test]
    fn design_example_parses_and_resolves_systems() {
        let fleet = parse::<FleetFile>(Path::new("fleet.toml"), DESIGN_EXAMPLE.as_bytes()).unwrap();
        assert_eq!(fleet.names(), vec!["box", "pi", "tower"]);
        let tower = fleet.machine("tower").unwrap();
        assert_eq!(tower.mac.as_deref(), Some("02:00:00:00:00:01"));
        assert_eq!(tower.wake_via, vec!["pi"]);
        let systems = tower.resolved_systems();
        assert_eq!(systems.len(), 2);
        assert!(systems[0].explicit);
        assert_eq!(systems[0].name, "linux");
        assert_eq!(systems[1].name, "windows");

        let pi = fleet.machine("pi").unwrap().resolved_systems();
        assert_eq!(pi.len(), 1);
        assert!(!pi[0].explicit);
        assert_eq!(pi[0].name, DEFAULT_SYSTEM_NAME);
    }

    #[test]
    fn validation_errors_name_the_machine_and_the_field() {
        let err = |text: &str| {
            parse::<FleetFile>(Path::new("fleet.toml"), text.as_bytes())
                .unwrap_err()
                .message
        };

        let msg = err("[machines.all]\nssh = \"pi\"\n");
        assert!(msg.contains("machines.all"), "{msg}");

        let msg = err("[machines.BadName]\nssh = \"pi\"\n");
        assert!(msg.contains("machines.BadName"), "{msg}");

        let msg = err("[machines.pi]\nssh = \"-oProxyCommand=bad\"\n");
        assert!(msg.contains("machines.pi") && msg.contains("ssh"), "{msg}");

        let msg = err("[machines.pi]\nssh = \"pi\"\ncommand = [\"legion\", \"rpc\"]\n");
        assert!(
            msg.contains("machines.pi") && msg.contains("ssh") && msg.contains("command"),
            "{msg}"
        );

        let msg = err("[machines.pi]\nmac = \"02:00:00:00:00:01\"\n");
        assert!(msg.contains("machines.pi") && msg.contains("ssh"), "{msg}");

        let msg = err("[machines.pi]\nssh = \"pi\"\nmac = \"not-a-mac\"\n");
        assert!(msg.contains("machines.pi") && msg.contains("mac"), "{msg}");

        let msg = err("[machines.pi]\nssh = \"pi\"\nwake_via = [\"pi\"]\n");
        assert!(
            msg.contains("machines.pi") && msg.contains("wake_via"),
            "{msg}"
        );

        let msg = err("[machines.pi]\nssh = \"pi\"\nprogram = \"/bad path/legion\"\n");
        assert!(
            msg.contains("machines.pi") && msg.contains("program"),
            "{msg}"
        );

        let msg = err("[machines.pi]\ncommand = [\"legion\", \"rpc\"]\nprogram = \"legion\"\n");
        assert!(
            msg.contains("machines.pi") && msg.contains("program"),
            "{msg}"
        );

        let msg = err("[machines.pi]\ncommand = []\n");
        assert!(
            msg.contains("machines.pi") && msg.contains("command"),
            "{msg}"
        );

        let msg = err("[machines.tower]\nsystems = []\n");
        assert!(
            msg.contains("machines.tower") && msg.contains("systems"),
            "{msg}"
        );

        let msg = err(
            "[machines.tower]\nos = \"unix\"\n[[machines.tower.systems]]\nname = \"linux\"\nssh = \"tower\"\n",
        );
        assert!(
            msg.contains("machines.tower") && msg.contains("os"),
            "{msg}"
        );

        let msg = err(
            "[[machines.tower.systems]]\nname = \"linux\"\nssh = \"a\"\n[[machines.tower.systems]]\nname = \"linux\"\nssh = \"b\"\n",
        );
        assert!(
            msg.contains("machines.tower") && msg.contains("systems"),
            "{msg}"
        );
    }

    #[test]
    fn program_and_ssh_target_and_mac_validation_are_exhaustive() {
        for ok in [
            "legion",
            "legion-dev",
            "/usr/local/legion/legion",
            "~/.local/bin/legion",
            r"C:\Users\me\legion.exe",
            "a_b.c:d-e/f~g",
        ] {
            assert!(validate_program(ok).is_ok(), "{ok} was rejected");
        }
        for bad in [
            "",
            "-legion",
            "--version",
            "legion ",
            "a b",
            "a'b",
            "a\"b",
            "a;b",
            "a|b",
            "a&b",
            "a$b",
            "a`b",
            "a(b)",
            "a\nb",
            "a\tb",
            r"C:\Tools\",
            "a*b",
            "a?b",
            "a>b",
            "a<b",
        ] {
            assert!(validate_program(bad).is_err(), "{bad:?} was accepted");
        }

        for ok in ["pi", "me@tower.example", "tower-win", "192.0.2.10"] {
            assert!(validate_ssh_target(ok).is_ok(), "{ok} was rejected");
        }
        for bad in ["", "-", "-oProxyCommand=x", "host name", "a\nb", "a\tb"] {
            assert!(validate_ssh_target(bad).is_err(), "{bad:?} was accepted");
        }

        for ok in [
            "02:00:00:00:00:01",
            "02-00-00-00-00-09",
            "AA:bb:CC:dd:EE:ff",
        ] {
            assert!(validate_mac(ok).is_ok(), "{ok} was rejected");
        }
        for bad in [
            "",
            "02:00:00:00:00",
            "02:00:00:00:00:01:02",
            "02:00-00:00:00:01",
            "02:00:00:00:00:zz",
            "020000000001",
        ] {
            assert!(validate_mac(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn upsert_and_remove_preserve_comments_and_unrelated_entries() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fleet.toml");
        std::fs::write(
            &file,
            "# Top-level note\n\n# Always-on Raspberry Pi\n[machines.pi]\nssh = \"pi.example\" # inline note\n",
        )
        .unwrap();

        let replaced = upsert_machine(
            &file,
            "tower",
            &MachineEntry {
                ssh: Some("tower.example".to_owned()),
                mac: Some("02:00:00:00:00:01".to_owned()),
                wake_via: vec!["pi".to_owned()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!replaced);

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("# Top-level note"), "{text}");
        assert!(text.contains("# Always-on Raspberry Pi"), "{text}");
        assert!(text.contains("# inline note"), "{text}");

        let replaced = upsert_machine(
            &file,
            "pi",
            &MachineEntry {
                ssh: Some("pi2.example".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(replaced);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("# Always-on Raspberry Pi"), "{text}");
        assert!(text.contains("pi2.example"), "{text}");

        remove_machine(&file, "tower").unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("# Top-level note"), "{text}");
        assert!(text.contains("# Always-on Raspberry Pi"), "{text}");
        assert!(!text.contains("tower.example"), "{text}");

        let missing = remove_machine(&file, "tower").unwrap_err();
        assert_eq!(missing.code, ErrorCode::NotFound);
    }
}
