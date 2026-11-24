//! The parts of `node.toml` the `power` tools read: which system this is, what keeps the machine
//! busy, how it sleeps and restarts, and which other systems it can restart into.
//!
//! ```toml
//! system = "linux"                     # which system of this machine this is
//!
//! busy = "none"                        # nothing here needs protecting; or describe what does:
//! # [[busy.probes]]
//! # name = "render"
//! # run = ["pgrep", "-x", "blender"]   # exit 0 busy, 1 idle, anything else unknown
//! # timeout_s = 5
//!
//! [power]
//! sudo = true                          # run system actions through `sudo -n` on Unix
//! # sleep = ["systemctl", "suspend"]   # replaces the platform's way to sleep
//! # before_sleep = [["some", "command"]]
//!
//! [boot.windows]                       # a system this machine can restart into
//! efi_label = "Windows Boot Manager"
//! ```

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use super::NodeName;

/// The longest a probe gets: `power.status` waits for the slowest probe, and a status that takes
/// minutes is one nobody waits for.
pub const LONGEST_PROBE_S: u32 = 60;

/// How long a probe gets when `node.toml` does not say.
pub const DEFAULT_PROBE_S: u32 = 5;

/// What keeps this machine busy, which is what `power.*` refuses to interrupt.
///
/// A `node.toml` with no `busy` setting leaves the machine *unmonitored*, which blocks unattended
/// power changes as surely as a busy probe does: not knowing whether the machine is busy is not
/// the same as knowing it is idle. Saying so takes one line:
///
/// ```toml
/// busy = "none"
/// ```
///
/// or a probe per thing worth protecting, each a program that exits 0 while the machine is busy
/// and 1 while it is idle; any other exit, or no answer within `timeout_s`, counts as busy:
///
/// ```toml
/// [[busy.probes]]
/// name = "render"
/// run = ["pgrep", "-x", "blender"]
/// timeout_s = 5
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusyConfig {
    /// `busy = "none"`: nothing on this machine needs protecting.
    Never,
    /// `[[busy.probes]]`: ask these programs.
    Probes(Vec<BusyProbe>),
}

/// One `[[busy.probes]]` entry.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BusyProbe {
    /// What the probe protects, as reasons name it: `render`, `backup`.
    pub name: String,
    /// The program and its arguments, run exactly as written with no shell: exit 0 means busy, 1
    /// means idle, anything else means unknown and counts as busy.
    pub run: Vec<String>,
    /// Seconds the probe may take, 1 to 60; 5 when absent. No answer by then counts as busy.
    #[serde(default = "default_probe_s")]
    pub timeout_s: u32,
}

fn default_probe_s() -> u32 {
    DEFAULT_PROBE_S
}

impl<'de> Deserialize<'de> for BusyConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The table form, read strictly so a misspelt key is an error.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            probes: Vec<BusyProbe>,
        }

        struct Shape;

        impl<'de> Visitor<'de> for Shape {
            type Value = BusyConfig;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("busy = \"none\", or [[busy.probes]] tables")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<BusyConfig, E> {
                if text == "none" {
                    Ok(BusyConfig::Never)
                } else {
                    Err(E::custom(format!(
                        "busy = {text:?} is not a setting: write busy = \"none\" when nothing on this machine needs protecting, or describe what does with [[busy.probes]]"
                    )))
                }
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<BusyConfig, A::Error> {
                let table = Table::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(BusyConfig::Probes(table.probes))
            }
        }

        deserializer.deserialize_any(Shape)
    }
}

/// The `[power]` table: how this machine sleeps and whether its system actions use `sudo`.
///
/// ```toml
/// [power]
/// sudo = true
/// sleep = ["systemctl", "hibernate"]
/// before_sleep = [["systemctl", "--user", "stop", "keep-awake.service"]]
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerConfig {
    /// Whether the built-in system actions on Unix (sleeping, restarting, shutting down, setting
    /// the firmware's next boot) run through `sudo -n`, the same meaning `[svc] sudo` has for
    /// services. Off unless set, so the node never escalates on its own; turning it on lets every
    /// principal allowed the power tools act as root through the sudoers rules for those commands.
    /// Commands written in this file run exactly as written either way.
    #[serde(default)]
    pub sudo: bool,
    /// The command that puts this machine to sleep, replacing the platform's own. Run exactly as
    /// written, a few seconds after `power.sleep` replies.
    #[serde(default)]
    pub sleep: Option<Vec<String>>,
    /// Commands run, in order, before the machine is put to sleep, for whatever holds sleep off
    /// while a session is open. A failure aborts the sleep.
    #[serde(default)]
    pub before_sleep: Vec<Vec<String>>,
}

/// One `[boot.<name>]` table: how this system arms the firmware to start the system called
/// `<name>` once, on the next boot.
///
/// The firmware entry with this label is looked up every time, never remembered, because firmware
/// renumbers its entries; the label must match the whole entry name, so it cannot pick a recovery
/// entry or an installer stick whose name merely contains it:
///
/// ```toml
/// [boot.windows]
/// efi_label = "Windows Boot Manager"
/// ```
///
/// or with commands of your own, where `verify` must exit 0 only when the arming took:
///
/// ```toml
/// [boot.windows]
/// arm = ["sudo", "-n", "grub-reboot", "Windows 11"]
/// verify = ["sh", "-c", "grub-editenv list | grep -qx 'next_entry=Windows 11'"]
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawBootTarget")]
pub enum BootTarget {
    /// The firmware entry with this label: `efibootmgr --bootnext` on Linux, `bcdedit` on Windows.
    EfiLabel(String),
    /// Commands of your own: `arm` sets the next boot, `verify` exits 0 only when it is set.
    Commands {
        /// Sets the next boot.
        arm: Vec<String>,
        /// Exits 0 only when the next boot is set; anything else means the arming did not take.
        verify: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBootTarget {
    #[serde(default)]
    efi_label: Option<String>,
    #[serde(default)]
    arm: Option<Vec<String>>,
    #[serde(default)]
    verify: Option<Vec<String>>,
}

impl TryFrom<RawBootTarget> for BootTarget {
    type Error = String;

    fn try_from(raw: RawBootTarget) -> Result<Self, Self::Error> {
        match (raw.efi_label, raw.arm, raw.verify) {
            (Some(label), None, None) => {
                if label.trim().is_empty() {
                    Err("efi_label cannot be empty".to_owned())
                } else {
                    Ok(BootTarget::EfiLabel(label))
                }
            }
            (None, Some(arm), Some(verify)) => {
                check_argv("arm", &arm)?;
                check_argv("verify", &verify)?;
                Ok(BootTarget::Commands { arm, verify })
            }
            (None, Some(_), None) => Err(
                "arm needs a verify command that exits 0 only when the arming took: a restart on the strength of an arming nobody read back lands in the system it was already in".to_owned(),
            ),
            (None, None, Some(_)) => Err("verify needs an arm command to verify".to_owned()),
            (Some(_), _, _) => {
                Err("give efi_label, or arm and verify, not both".to_owned())
            }
            (None, None, None) => Err(
                "say how to start this system: efi_label = \"<firmware entry>\", or arm and verify commands".to_owned(),
            ),
        }
    }
}

fn check_argv(what: &str, argv: &[String]) -> Result<(), String> {
    match argv.first() {
        Some(program) if !program.trim().is_empty() => Ok(()),
        _ => Err(format!(
            "{what} must be a program and its arguments, such as [\"systemctl\", \"suspend\"]"
        )),
    }
}

/// Checks what the types cannot: names, argv, ranges and duplicates. Each message names the key.
pub(super) fn validate(
    busy: Option<&BusyConfig>,
    power: &PowerConfig,
    boot: &BTreeMap<String, BootTarget>,
) -> Result<(), String> {
    if let Some(BusyConfig::Probes(probes)) = busy {
        if probes.is_empty() {
            return Err(
                "busy.probes is empty: write busy = \"none\" when nothing on this machine needs protecting"
                    .to_owned(),
            );
        }
        let mut names = HashSet::new();
        for (index, probe) in probes.iter().enumerate() {
            let at = format!("busy.probes[{index}]");
            if probe.name.trim().is_empty() || probe.name.chars().any(char::is_control) {
                return Err(format!("{at}.name must be a short name such as \"render\""));
            }
            if !names.insert(probe.name.as_str()) {
                return Err(format!(
                    "{at}.name: another probe is already called {:?}",
                    probe.name
                ));
            }
            check_argv(&format!("{at}.run"), &probe.run)?;
            if !(1..=LONGEST_PROBE_S).contains(&probe.timeout_s) {
                return Err(format!(
                    "{at}.timeout_s must be 1 to {LONGEST_PROBE_S} seconds, not {}",
                    probe.timeout_s
                ));
            }
        }
    }
    if let Some(sleep) = &power.sleep {
        check_argv("power.sleep", sleep)?;
    }
    for (index, command) in power.before_sleep.iter().enumerate() {
        check_argv(&format!("power.before_sleep[{index}]"), command)?;
    }
    for name in boot.keys() {
        NodeName::try_from(name.clone()).map_err(|reason| format!("boot.{name}: {reason}"))?;
    }
    Ok(())
}
