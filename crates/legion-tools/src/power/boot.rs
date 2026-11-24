//! Arming the firmware to start another system once: resolve the entry, arm it, read it back.
//!
//! The entry is looked up by its label on every call and never remembered, because firmware
//! renumbers its entries. The label must match an entry's whole name, so `Windows Boot Manager`
//! cannot pick `UEFI: Windows Boot Manager USB` (an installer stick) or a recovery entry, either of
//! which comes up in a menu with no network. After arming, the setting is read back from the
//! firmware and compared; only a match lets the restart go ahead.

use std::time::Duration;

use legion_core::CancellationToken;
use legion_core::config::BootTarget;
use legion_core::proto::{ErrorBody, ErrorCode};
use serde_json::json;

use super::actuator::{Actuator, Ran, program};
use super::plan::{Platform, looks_refused, maybe_sudo, refused, strings, with_sudo};

/// How long reading the firmware's entries may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);
/// How long an arm or verify command may take.
const ARM_TIMEOUT: Duration = Duration::from_secs(30);

/// Device paths efibootmgr older than version 18 prints after a label with only spaces between.
const DEVICE_PATH_STARTS: [&str; 9] = [
    "HD(",
    "PciRoot(",
    "PcieRoot(",
    "VenHw(",
    "VenMedia(",
    "Fv(",
    "BBS(",
    "UsbClass(",
    "MAC(",
];

/// One `Boot####` entry from `efibootmgr`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EfiEntry {
    /// The four hex digits, upper case.
    pub number: String,
    /// The label, without the device path.
    pub label: String,
    /// Whether the entry is active (`*`); firmware may ignore a `BootNext` to an inactive one.
    pub active: bool,
}

/// What `efibootmgr` lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EfiListing {
    /// Every `Boot####` entry.
    pub entries: Vec<EfiEntry>,
    /// The entry armed for the next boot, if any.
    pub boot_next: Option<String>,
}

/// Reads `efibootmgr` output, from version 18's tab-separated form and older ones alike.
pub fn parse_efibootmgr(text: &str) -> EfiListing {
    let mut listing = EfiListing::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("BootNext:") {
            let number = rest.trim();
            if is_boot_number(number) {
                listing.boot_next = Some(number.to_ascii_uppercase());
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("Boot") else {
            continue;
        };
        let Some(number) = rest.get(..4).filter(|number| is_boot_number(number)) else {
            continue;
        };
        let rest = &rest[4..];
        let (active, rest) = match rest.strip_prefix('*') {
            Some(after) => (true, after),
            None => (false, rest),
        };
        if !rest.starts_with([' ', '\t']) {
            continue;
        }
        listing.entries.push(EfiEntry {
            number: number.to_ascii_uppercase(),
            label: efi_label(rest),
            active,
        });
    }
    listing
}

fn is_boot_number(text: &str) -> bool {
    text.len() == 4 && text.chars().all(|c| c.is_ascii_hexdigit())
}

fn efi_label(rest: &str) -> String {
    let before_tab = rest.split('\t').next().unwrap_or("").trim_start();
    let cut = DEVICE_PATH_STARTS
        .iter()
        .filter_map(|start| {
            before_tab
                .match_indices(start)
                .map(|(at, _)| at)
                .find(|&at| at > 0 && before_tab[..at].ends_with(char::is_whitespace))
        })
        .min();
    match cut {
        Some(at) => before_tab[..at].trim().to_owned(),
        None => before_tab.trim().to_owned(),
    }
}

/// One firmware application `bcdedit /enum firmware` lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BcdEntry {
    /// Its identifier: `{bootmgr}` or a `{guid}`.
    pub id: String,
    /// Its `description`, when it has one.
    pub label: Option<String>,
}

/// Reads `bcdedit /enum` output into its entries.
///
/// bcdedit translates the word `identifier` (German Windows says `Bezeichner`) but not the names
/// of the elements themselves, so the identifier is taken as each entry's first line, and the
/// label from its `description` element.
pub fn parse_bcdedit(text: &str) -> Vec<BcdEntry> {
    let mut entries = Vec::new();
    let mut current: Option<BcdEntry> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && trimmed.chars().all(|c| c == '-') {
            entries.extend(current.take());
            current = Some(BcdEntry {
                id: String::new(),
                label: None,
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if trimmed.is_empty() || line.starts_with(char::is_whitespace) {
            continue;
        }
        let (key, value) = split_property(trimmed);
        if entry.id.is_empty() {
            entry.id = value.to_owned();
        } else if key.eq_ignore_ascii_case("description") {
            entry.label = Some(value.to_owned());
        }
    }
    entries.extend(current);
    entries.retain(|entry| entry.id.starts_with('{'));
    entries
}

/// The value of `bootsequence` in `bcdedit /enum {fwbootmgr}` output: the entries the firmware
/// starts next, first one first.
pub fn parse_bootsequence(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if line.starts_with(char::is_whitespace) && inside && !trimmed.is_empty() {
            values.push(trimmed.to_owned());
            continue;
        }
        let (key, value) = split_property(trimmed);
        inside = key.eq_ignore_ascii_case("bootsequence");
        if inside && !value.is_empty() {
            values.push(value.to_owned());
        }
    }
    values
}

fn split_property(line: &str) -> (&str, &str) {
    match line.split_once(char::is_whitespace) {
        Some((key, value)) => (key, value.trim()),
        None => (line, ""),
    }
}

/// Which entries carry exactly this label, compared whole and without regard to case.
fn matching<'a, T>(entries: &'a [T], label: &str, of: impl Fn(&T) -> Option<&str>) -> Vec<&'a T> {
    let wanted = label.trim();
    entries
        .iter()
        .filter(|entry| of(entry).is_some_and(|have| have.trim().eq_ignore_ascii_case(wanted)))
        .collect()
}

/// What arming a target takes, as resolved just now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arming {
    /// The command that arms it.
    pub arm: Vec<String>,
    /// How the arming is read back.
    pub readback: Readback,
    /// The firmware entry chosen, for messages, when there is one.
    pub entry: Option<String>,
    /// The command that clears the arming again, when legion knows one: arm and verify commands
    /// from node.toml have none.
    pub disarm: Option<Vec<String>>,
}

/// How an arming is confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readback {
    /// `efibootmgr` must report `BootNext` as this entry.
    BootNext(String),
    /// `bcdedit /enum {fwbootmgr}` must report `bootsequence` starting with this entry.
    BootSequence(String),
    /// This command must exit 0.
    Verify(Vec<String>),
}

/// How a target is armed on this platform, in a phrase for `power.status`.
pub fn describe(platform: Platform, target: &BootTarget) -> String {
    match (target, platform) {
        (BootTarget::Commands { .. }, _) => "the arm and verify commands in node.toml".to_owned(),
        (BootTarget::EfiLabel(label), Platform::Linux) => {
            format!("efibootmgr --bootnext, firmware entry labelled {label:?}")
        }
        (BootTarget::EfiLabel(label), Platform::Windows) => {
            format!("bcdedit {{fwbootmgr}} bootsequence, firmware entry labelled {label:?}")
        }
        (BootTarget::EfiLabel(_), Platform::Macos) => {
            "nothing: efi_label needs efibootmgr (Linux) or bcdedit (Windows), and this is macOS"
                .to_owned()
        }
    }
}

/// Whether this platform can arm the target at all, known without asking the machine anything.
pub fn check_supported(
    platform: Platform,
    name: &str,
    target: &BootTarget,
) -> Result<(), ErrorBody> {
    match (platform, target) {
        (Platform::Macos, BootTarget::EfiLabel(_)) => Err(no_firmware_label_on_macos(name)),
        _ => Ok(()),
    }
}

fn no_firmware_label_on_macos(name: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Unsupported,
        format!("Boot target {name} names a firmware entry, and macOS has no way for legion to set the firmware's next boot."),
    )
    .with_hint(format!(
        "Give [boot.{name}] arm and verify commands instead of efi_label in node.toml."
    ))
}

/// Looks the target's entry up now and says what arming it takes. Reads, changes nothing.
pub async fn prepare(
    actuator: &dyn Actuator,
    cancel: &CancellationToken,
    platform: Platform,
    sudo: bool,
    name: &str,
    target: &BootTarget,
) -> Result<Arming, ErrorBody> {
    let label = match target {
        BootTarget::Commands { arm, verify } => {
            return Ok(Arming {
                arm: arm.clone(),
                readback: Readback::Verify(verify.clone()),
                entry: None,
                disarm: None,
            });
        }
        BootTarget::EfiLabel(label) => label,
    };
    match platform {
        Platform::Macos => Err(no_firmware_label_on_macos(name)),
        Platform::Linux => {
            let listing = read_efi(actuator, cancel, sudo).await?;
            let found = matching(&listing.entries, label, |entry| Some(&entry.label));
            let entry = match found.as_slice() {
                [one] => *one,
                [] => {
                    let labels: Vec<&str> =
                        listing.entries.iter().map(|e| e.label.as_str()).collect();
                    return Err(no_entry(name, label, &labels));
                }
                several => {
                    let numbers: Vec<String> = several
                        .iter()
                        .map(|e| format!("Boot{}", e.number))
                        .collect();
                    return Err(several_entries(name, label, &numbers));
                }
            };
            if !entry.active {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("The firmware entry Boot{} {label:?} is inactive, and firmware may ignore a next boot set to it.", entry.number),
                )
                .with_hint(format!(
                    "Activate it on the machine with `efibootmgr --bootnum {} --active` (as root), then ask again.",
                    entry.number
                )));
            }
            Ok(Arming {
                arm: maybe_sudo(
                    platform,
                    sudo,
                    strings(&["efibootmgr", "--bootnext", &entry.number]),
                ),
                readback: Readback::BootNext(entry.number.clone()),
                entry: Some(format!("Boot{} {:?}", entry.number, entry.label)),
                disarm: Some(maybe_sudo(
                    platform,
                    sudo,
                    strings(&["efibootmgr", "--delete-bootnext"]),
                )),
            })
        }
        Platform::Windows => {
            let listing = run_read(
                actuator,
                cancel,
                &strings(&["bcdedit", "/enum", "firmware"]),
                platform,
                false,
                "read the firmware's boot entries",
            )
            .await?;
            let entries = parse_bcdedit(&listing.stdout);
            let found = matching(&entries, label, |entry| entry.label.as_deref());
            let entry = match found.as_slice() {
                [one] => *one,
                [] => {
                    let labels: Vec<&str> =
                        entries.iter().filter_map(|e| e.label.as_deref()).collect();
                    return Err(no_entry(name, label, &labels));
                }
                several => {
                    let ids: Vec<String> = several.iter().map(|e| e.id.clone()).collect();
                    return Err(several_entries(name, label, &ids));
                }
            };
            Ok(Arming {
                arm: strings(&["bcdedit", "/set", "{fwbootmgr}", "bootsequence", &entry.id]),
                readback: Readback::BootSequence(entry.id.clone()),
                entry: Some(format!("{} {label:?}", entry.id)),
                disarm: Some(strings(&[
                    "bcdedit",
                    "/deletevalue",
                    "{fwbootmgr}",
                    "bootsequence",
                ])),
            })
        }
    }
}

/// Arms the target and reads the setting back; an error means the restart must not happen.
pub async fn arm(
    actuator: &dyn Actuator,
    cancel: &CancellationToken,
    platform: Platform,
    sudo: bool,
    name: &str,
    arming: &Arming,
) -> Result<(), ErrorBody> {
    let what = format!("set the firmware to start {name} next");
    let ran = actuator.run(&arming.arm, ARM_TIMEOUT, cancel).await?;
    if !ran.ok() {
        let said = said(&ran);
        if was_refused(actuator, cancel, platform, &ran, &said).await {
            return Err(refused(platform, &what, sudo, &arming.arm, &said));
        }
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Could not {what}: `{}` {}. Nothing was restarted.",
                arming.arm.join(" "),
                failure(&ran)
            ),
        )
        .with_details(json!({"argv": arming.arm, "said": said})));
    }
    let nothing_restarted = |found: String| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("The firmware did not keep the setting to start {name} next: {found}. Nothing was restarted."),
        )
        .with_hint("A restart now would come back to the system already running. Check the firmware's boot entries on the machine.")
    };
    match &arming.readback {
        Readback::BootNext(expected) => {
            let listing = read_efi(actuator, cancel, sudo).await?;
            match &listing.boot_next {
                Some(found) if found == expected => Ok(()),
                found => Err(nothing_restarted(format!(
                    "BootNext reads {}, not {expected}",
                    found.as_deref().unwrap_or("unset")
                ))
                .with_details(json!({"expected": expected, "found": found}))),
            }
        }
        Readback::BootSequence(expected) => {
            let back = run_read(
                actuator,
                cancel,
                &strings(&["bcdedit", "/enum", "{fwbootmgr}"]),
                platform,
                false,
                "read the firmware's next boot back",
            )
            .await?;
            let sequence = parse_bootsequence(&back.stdout);
            match sequence.first() {
                Some(found) if found.eq_ignore_ascii_case(expected) => Ok(()),
                found => Err(nothing_restarted(format!(
                    "bootsequence reads {}, not {expected}",
                    found.map_or("unset", String::as_str)
                ))
                .with_details(json!({"expected": expected, "found": sequence}))),
            }
        }
        Readback::Verify(verify) => {
            let checked = actuator.run(verify, ARM_TIMEOUT, cancel).await?;
            if checked.ok() {
                Ok(())
            } else {
                Err(nothing_restarted(format!(
                    "the verify command `{}` {}",
                    verify.join(" "),
                    failure(&checked)
                ))
                .with_details(json!({"argv": verify, "said": said(&checked)})))
            }
        }
    }
}

/// `efibootmgr`'s listing: as this account first, then through sudo when that is allowed and the
/// firmware variables are readable only by root.
async fn read_efi(
    actuator: &dyn Actuator,
    cancel: &CancellationToken,
    sudo: bool,
) -> Result<EfiListing, ErrorBody> {
    let plain = strings(&["efibootmgr"]);
    let mut ran = actuator.run(&plain, LIST_TIMEOUT, cancel).await?;
    if !ran.ok() && sudo {
        ran = actuator
            .run(&with_sudo(&plain), LIST_TIMEOUT, cancel)
            .await?;
    }
    if ran.ok() {
        return Ok(parse_efibootmgr(&ran.stdout));
    }
    let said = said(&ran);
    if said.to_ascii_lowercase().contains("not supported") {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!("This machine's firmware boot entries cannot be read: {said}."),
        )
        .with_hint("efi_label works only on a machine started through UEFI; give the boot target arm and verify commands instead."));
    }
    if looks_refused(&said) {
        return Err(refused(
            Platform::Linux,
            "read the firmware's boot entries",
            sudo,
            &if sudo { with_sudo(&plain) } else { plain },
            &said,
        ));
    }
    Err(ErrorBody::new(
        ErrorCode::Failed,
        format!("efibootmgr could not read the firmware's boot entries: {said}."),
    ))
}

async fn run_read(
    actuator: &dyn Actuator,
    cancel: &CancellationToken,
    argv: &[String],
    platform: Platform,
    sudo: bool,
    what: &str,
) -> Result<Ran, ErrorBody> {
    let ran = actuator.run(argv, LIST_TIMEOUT, cancel).await?;
    if ran.ok() {
        return Ok(ran);
    }
    let said = said(&ran);
    if was_refused(actuator, cancel, platform, &ran, &said).await {
        return Err(refused(platform, what, sudo, argv, &said));
    }
    Err(ErrorBody::new(
        ErrorCode::Failed,
        format!("Could not {what}: `{}` {}.", program(argv), failure(&ran)),
    )
    .with_details(json!({"argv": argv, "said": said})))
}

/// Whether a failed command was the system refusing this account.
///
/// On Windows that is decided by asking whether this process is elevated, not by the words:
/// bcdedit says "Access is denied." in English only, and exits 1 for that as for most other
/// failures. Only when the question cannot be answered are the exit code and the words used.
async fn was_refused(
    actuator: &dyn Actuator,
    cancel: &CancellationToken,
    platform: Platform,
    ran: &Ran,
    said: &str,
) -> bool {
    if platform == Platform::Windows {
        let probe = actuator.run(&elevation_probe(), LIST_TIMEOUT, cancel).await;
        match probe
            .ok()
            .filter(Ran::ok)
            .map(|ran| ran.stdout.trim().to_owned())
        {
            Some(answer) if answer.eq_ignore_ascii_case("false") => return true,
            Some(answer) if answer.eq_ignore_ascii_case("true") => return false,
            _ => return ran.exit_code == Some(5) || looks_refused(said),
        }
    }
    looks_refused(said)
}

/// Asks Windows whether this process runs elevated: `True` or `False`, the same in every language.
pub fn elevation_probe() -> Vec<String> {
    strings(&[
        "powershell.exe",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
    ])
}

fn said(ran: &Ran) -> String {
    let said = ran.said();
    if said.is_empty() {
        "no message".to_owned()
    } else {
        said
    }
}

fn failure(ran: &Ran) -> String {
    match (ran.timed_out, ran.exit_code) {
        (true, _) => "did not finish in time".to_owned(),
        (false, Some(code)) => format!("exited {code} ({})", said(ran)),
        (false, None) => "was stopped by a signal".to_owned(),
    }
}

fn no_entry(name: &str, label: &str, labels: &[&str]) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::NotFound,
        format!(
            "No firmware boot entry is labelled {label:?}, which [boot.{name}] in node.toml names."
        ),
    )
    .with_hint(format!(
        "Set efi_label to the whole name of one of these entries: {}.",
        labels
            .iter()
            .map(|label| format!("{label:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .with_details(json!({"labels": labels}))
}

fn several_entries(name: &str, label: &str, ids: &[String]) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Conflict,
        format!(
            "Several firmware boot entries are labelled {label:?} ({}), so [boot.{name}] cannot tell which one is meant.",
            ids.join(", ")
        ),
    )
    .with_hint("Remove the stale entries or give the right one a label of its own, or use arm and verify commands for this target.")
    .with_details(json!({"entries": ids}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::fake::{RecordingActuator, Reply};

    /// What efibootmgr 18 prints on a dual boot machine with a few leftovers, device paths cut
    /// short; labels that contain the one wanted, and two entries sharing a label.
    const EFIBOOTMGR: &str = "BootCurrent: 0001\n\
Timeout: 2 seconds\n\
BootOrder: 0001,0004,0000,0005,0006\n\
Boot0000* Windows Boot Manager\tHD(1,GPT,0f5b7c1e-2d1a-4c8e-9b3f-5a6d7e8f9012,0x800,0x100000)/\\EFI\\Microsoft\\Boot\\bootmgfw.efi57494e444f5753000100000088\n\
Boot0001* Linux Boot Manager\tHD(1,GPT,0f5b7c1e-2d1a-4c8e-9b3f-5a6d7e8f9012,0x800,0x100000)/\\EFI\\systemd\\systemd-bootx64.efi\n\
Boot0002  Windows Boot Manager (recovery)\tHD(3,GPT,1a2b3c4d-5e6f-4a1b-8c2d-3e4f5a6b7c8d,0x80000,0x200000)/\\EFI\\Recovery\\bootx64.efi\n\
Boot0003* UEFI: Windows Boot Manager USB\tPciRoot(0x0)/Pci(0x14,0x0)/USB(3,0)/HD(1,MBR,0x1234,0x800,0x1000)\n\
Boot0004* UEFI OS\tHD(1,GPT,0f5b7c1e-2d1a-4c8e-9b3f-5a6d7e8f9012,0x800,0x100000)/\\EFI\\BOOT\\BOOTX64.EFI0000424f\n\
Boot0005* UEFI OS\tHD(2,GPT,2b3c4d5e-6f7a-4b2c-9d3e-4f5a6b7c8d9e,0x100800,0x200000)/\\EFI\\BOOT\\BOOTX64.EFI0000424f\n\
Boot000A* UEFI: PXE IPv4 Ethernet\tPciRoot(0x0)/Pci(0x1c,0x0)/MAC(020000000001,0)/IPv4(0.0.0.0,0,DHCP)\n";

    #[test]
    fn efibootmgr_entries_are_read_with_their_numbers_labels_and_state() {
        let listing = parse_efibootmgr(EFIBOOTMGR);
        assert_eq!(listing.boot_next, None);
        let labels: Vec<(&str, &str, bool)> = listing
            .entries
            .iter()
            .map(|e| (e.number.as_str(), e.label.as_str(), e.active))
            .collect();
        assert_eq!(
            labels,
            [
                ("0000", "Windows Boot Manager", true),
                ("0001", "Linux Boot Manager", true),
                ("0002", "Windows Boot Manager (recovery)", false),
                ("0003", "UEFI: Windows Boot Manager USB", true),
                ("0004", "UEFI OS", true),
                ("0005", "UEFI OS", true),
                ("000A", "UEFI: PXE IPv4 Ethernet", true),
            ]
        );
        let armed = parse_efibootmgr("BootCurrent: 0001\nBootNext: 000a\nBoot000A* x\n");
        assert_eq!(armed.boot_next.as_deref(), Some("000A"));
    }

    #[test]
    fn older_efibootmgr_output_without_tabs_is_read_too() {
        let listing = parse_efibootmgr(
            "BootNext: 0000\nBoot0000* Windows Boot Manager HD(1,GPT,0f5b7c1e,0x800,0x100000)/File(\\EFI\\Microsoft\\Boot\\bootmgfw.efi)\nBoot0001* ubuntu\n",
        );
        assert_eq!(listing.entries[0].label, "Windows Boot Manager");
        assert_eq!(listing.entries[1].label, "ubuntu");
        assert_eq!(listing.boot_next.as_deref(), Some("0000"));
    }

    #[test]
    fn a_label_matches_whole_names_only() {
        let listing = parse_efibootmgr(EFIBOOTMGR);
        let numbers = |wanted: &str| -> Vec<String> {
            matching(&listing.entries, wanted, |e| Some(e.label.as_str()))
                .iter()
                .map(|e| e.number.clone())
                .collect()
        };
        assert_eq!(numbers("Windows Boot Manager"), ["0000"]);
        assert_eq!(numbers("windows boot manager "), ["0000"]);
        assert_eq!(numbers("Linux Boot Manager"), ["0001"]);
        assert_eq!(numbers("UEFI OS"), ["0004", "0005"]);
        assert!(numbers("Windows").is_empty());
        assert!(numbers("Boot Manager").is_empty());
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        strings(parts)
    }

    #[tokio::test]
    async fn linux_arms_the_resolved_entry_and_reads_it_back() {
        let armed = EFIBOOTMGR.replace("Timeout:", "BootNext: 0000\nTimeout:");
        let fake = RecordingActuator::new()
            .reply(Reply::new(&["efibootmgr"], 0, EFIBOOTMGR))
            .reply(Reply::new(&["efibootmgr"], 0, &armed));
        let cancel = CancellationToken::new();
        let target = BootTarget::EfiLabel("Windows Boot Manager".to_owned());
        let arming = prepare(&fake, &cancel, Platform::Linux, true, "windows", &target)
            .await
            .unwrap();
        assert_eq!(
            arming.arm,
            argv(&["sudo", "-n", "efibootmgr", "--bootnext", "0000"])
        );
        assert_eq!(arming.readback, Readback::BootNext("0000".to_owned()));
        assert_eq!(
            arming.disarm,
            Some(argv(&["sudo", "-n", "efibootmgr", "--delete-bootnext"]))
        );
        arm(&fake, &cancel, Platform::Linux, true, "windows", &arming)
            .await
            .unwrap();
        assert_eq!(
            fake.runs(),
            [
                argv(&["efibootmgr"]),
                argv(&["sudo", "-n", "efibootmgr", "--bootnext", "0000"]),
                argv(&["efibootmgr"]),
            ]
        );
    }

    #[tokio::test]
    async fn a_read_back_that_does_not_match_fails_and_says_nothing_restarted() {
        let wrong = EFIBOOTMGR.replace("Timeout:", "BootNext: 0004\nTimeout:");
        let fake = RecordingActuator::new()
            .reply(Reply::new(&["efibootmgr"], 0, EFIBOOTMGR))
            .reply(Reply::new(&["efibootmgr"], 0, &wrong));
        let cancel = CancellationToken::new();
        let target = BootTarget::EfiLabel("Windows Boot Manager".to_owned());
        let arming = prepare(&fake, &cancel, Platform::Linux, false, "windows", &target)
            .await
            .unwrap();
        assert_eq!(arming.arm, argv(&["efibootmgr", "--bootnext", "0000"]));
        let error = arm(&fake, &cancel, Platform::Linux, false, "windows", &arming)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Failed);
        assert!(
            error.message.contains("BootNext reads 0004, not 0000"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("Nothing was restarted"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn linux_refuses_a_missing_ambiguous_or_inactive_entry() {
        let cancel = CancellationToken::new();
        let fake = RecordingActuator::new().reply(Reply::new(&["efibootmgr"], 0, EFIBOOTMGR));
        let prepare_label = |label: &str| {
            let target = BootTarget::EfiLabel(label.to_owned());
            let fake = &fake;
            let cancel = &cancel;
            async move { prepare(fake, cancel, Platform::Linux, false, "other", &target).await }
        };
        let missing = prepare_label("Windows").await.unwrap_err();
        assert_eq!(missing.code, ErrorCode::NotFound);
        assert!(missing.hint.unwrap().contains("\"Windows Boot Manager\""));
        let several = prepare_label("UEFI OS").await.unwrap_err();
        assert_eq!(several.code, ErrorCode::Conflict);
        assert!(
            several.message.contains("Boot0004, Boot0005"),
            "{}",
            several.message
        );
        let inactive = prepare_label("Windows Boot Manager (recovery)")
            .await
            .unwrap_err();
        assert_eq!(inactive.code, ErrorCode::Failed);
        assert!(inactive.hint.unwrap().contains("--bootnum 0002 --active"));
    }

    #[tokio::test]
    async fn a_refused_arm_is_denied_with_the_sudo_setting_named() {
        let fake = RecordingActuator::new()
            .reply(Reply::new(&["efibootmgr"], 0, EFIBOOTMGR))
            .reply(Reply {
                stderr: "Could not set BootNext: Permission denied\n".to_owned(),
                ..Reply::new(&["efibootmgr", "--bootnext"], 5, "")
            });
        let cancel = CancellationToken::new();
        let target = BootTarget::EfiLabel("Windows Boot Manager".to_owned());
        let arming = prepare(&fake, &cancel, Platform::Linux, false, "windows", &target)
            .await
            .unwrap();
        let error = arm(&fake, &cancel, Platform::Linux, false, "windows", &arming)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied);
        assert!(error.hint.unwrap().contains("sudo = true under [power]"));
        assert_eq!(
            error.details.unwrap()["argv"],
            json!(["sudo", "-n", "efibootmgr", "--bootnext", "0000"])
        );
    }

    #[tokio::test]
    async fn firmware_without_efi_variables_is_unsupported() {
        let unsupported = |argv: &[&str]| Reply {
            stderr: "EFI variables are not supported on this system.\n".to_owned(),
            ..Reply::new(argv, 2, "")
        };
        let fake = RecordingActuator::new()
            .reply(unsupported(&["efibootmgr"]))
            .reply(unsupported(&["sudo", "-n", "efibootmgr"]));
        let cancel = CancellationToken::new();
        let target = BootTarget::EfiLabel("Windows Boot Manager".to_owned());
        let error = prepare(&fake, &cancel, Platform::Linux, true, "windows", &target)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Unsupported);
        // With sudo allowed, the read was tried through it as well.
        assert_eq!(
            fake.runs(),
            [argv(&["efibootmgr"]), argv(&["sudo", "-n", "efibootmgr"])]
        );
    }

    /// `bcdedit /enum firmware` on an English Windows, trimmed.
    const BCDEDIT_FIRMWARE: &str = "\r\n\
Firmware Boot Manager\r\n\
---------------------\r\n\
identifier              {fwbootmgr}\r\n\
displayorder            {bootmgr}\r\n\
                        {7a1c5e2b-3d4f-4a6b-8c9d-0e1f2a3b4c5d}\r\n\
                        {8b2d6f3c-4e5a-4b7c-9d0e-1f2a3b4c5d6e}\r\n\
timeout                 0\r\n\
\r\n\
Windows Boot Manager\r\n\
--------------------\r\n\
identifier              {bootmgr}\r\n\
device                  partition=\\Device\\HarddiskVolume1\r\n\
path                    \\EFI\\Microsoft\\Boot\\bootmgfw.efi\r\n\
description             Windows Boot Manager\r\n\
locale                  en-US\r\n\
\r\n\
Firmware Application (101fffff)\r\n\
-------------------------------\r\n\
identifier              {7a1c5e2b-3d4f-4a6b-8c9d-0e1f2a3b4c5d}\r\n\
device                  partition=\\Device\\HarddiskVolume1\r\n\
path                    \\EFI\\systemd\\systemd-bootx64.efi\r\n\
description             Linux Boot Manager\r\n\
\r\n\
Firmware Application (101fffff)\r\n\
-------------------------------\r\n\
identifier              {8b2d6f3c-4e5a-4b7c-9d0e-1f2a3b4c5d6e}\r\n\
description             UEFI: PXE IPv4 Ethernet\r\n";

    #[test]
    fn bcdedit_entries_are_read_in_any_language() {
        let entries = parse_bcdedit(BCDEDIT_FIRMWARE);
        let pairs: Vec<(&str, Option<&str>)> = entries
            .iter()
            .map(|e| (e.id.as_str(), e.label.as_deref()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("{fwbootmgr}", None),
                ("{bootmgr}", Some("Windows Boot Manager")),
                (
                    "{7a1c5e2b-3d4f-4a6b-8c9d-0e1f2a3b4c5d}",
                    Some("Linux Boot Manager")
                ),
                (
                    "{8b2d6f3c-4e5a-4b7c-9d0e-1f2a3b4c5d6e}",
                    Some("UEFI: PXE IPv4 Ethernet")
                ),
            ]
        );
        let german = BCDEDIT_FIRMWARE
            .replace("identifier  ", "Bezeichner  ")
            .replace("Firmware Application", "Firmwareanwendung");
        assert_eq!(parse_bcdedit(&german), entries);
    }

    #[test]
    fn bootsequence_is_read_with_its_continuation_lines() {
        let text = "Firmware Boot Manager\n---------------------\nidentifier              {fwbootmgr}\ndisplayorder            {bootmgr}\nbootsequence            {7a1c5e2b-3d4f-4a6b-8c9d-0e1f2a3b4c5d}\n                        {bootmgr}\ntimeout                 0\n";
        assert_eq!(
            parse_bootsequence(text),
            ["{7a1c5e2b-3d4f-4a6b-8c9d-0e1f2a3b4c5d}", "{bootmgr}"]
        );
        assert!(parse_bootsequence("identifier {fwbootmgr}\ntimeout 0\n").is_empty());
    }

    #[tokio::test]
    async fn windows_sets_bootsequence_and_compares_it() {
        let after = "identifier              {fwbootmgr}\r\nbootsequence            {7A1C5E2B-3D4F-4A6B-8C9D-0E1F2A3B4C5D}\r\n";
        let fake = RecordingActuator::new()
            .reply(Reply::new(
                &["bcdedit", "/enum", "firmware"],
                0,
                BCDEDIT_FIRMWARE,
            ))
            .reply(Reply::new(&["bcdedit", "/enum", "{fwbootmgr}"], 0, after));
        let cancel = CancellationToken::new();
        let target = BootTarget::EfiLabel("Linux Boot Manager".to_owned());
        // sudo means nothing on Windows.
        let arming = prepare(&fake, &cancel, Platform::Windows, true, "linux", &target)
            .await
            .unwrap();
        assert_eq!(
            arming.arm,
            argv(&[
                "bcdedit",
                "/set",
                "{fwbootmgr}",
                "bootsequence",
                "{7a1c5e2b-3d4f-4a6b-8c9d-0e1f2a3b4c5d}"
            ])
        );
        assert_eq!(
            arming.disarm,
            Some(argv(&[
                "bcdedit",
                "/deletevalue",
                "{fwbootmgr}",
                "bootsequence"
            ]))
        );
        arm(&fake, &cancel, Platform::Windows, true, "linux", &arming)
            .await
            .unwrap();
        assert_eq!(fake.runs().len(), 3);

        let stale = RecordingActuator::new()
            .reply(Reply::new(
                &["bcdedit", "/enum", "firmware"],
                0,
                BCDEDIT_FIRMWARE,
            ))
            .reply(Reply::new(
                &["bcdedit", "/enum", "{fwbootmgr}"],
                0,
                "identifier {fwbootmgr}\r\n",
            ));
        let arming = prepare(&stale, &cancel, Platform::Windows, false, "linux", &target)
            .await
            .unwrap();
        let error = arm(&stale, &cancel, Platform::Windows, false, "linux", &arming)
            .await
            .unwrap_err();
        assert!(
            error.message.contains("bootsequence reads unset"),
            "{}",
            error.message
        );

        let denied = RecordingActuator::new()
            .reply(Reply::new(&["bcdedit", "/enum", "firmware"], 0, BCDEDIT_FIRMWARE))
            .reply(Reply {
                stdout: "The boot configuration data store could not be opened.\r\nAccess is denied.\r\n".to_owned(),
                ..Reply::new(&["bcdedit", "/set"], 1, "")
            });
        let arming = prepare(&denied, &cancel, Platform::Windows, false, "linux", &target)
            .await
            .unwrap();
        let error = arm(&denied, &cancel, Platform::Windows, false, "linux", &arming)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied);
        assert!(error.hint.unwrap().contains("elevated"));
    }

    #[tokio::test]
    async fn a_windows_refusal_is_known_by_the_token_not_the_words() {
        let target = BootTarget::EfiLabel("Linux Boot Manager".to_owned());
        let cancel = CancellationToken::new();
        // A German Windows: bcdedit exits 1 and says it in German.
        let german = |elevated: &str| {
            RecordingActuator::new()
                .reply(Reply::new(&["bcdedit", "/enum", "firmware"], 0, BCDEDIT_FIRMWARE))
                .reply(Reply::new(
                    &["bcdedit", "/set"],
                    1,
                    "Der Speicher für die Startkonfigurationsdaten konnte nicht geöffnet werden.\r\nZugriff verweigert\r\n",
                ))
                .reply(Reply::new(&["powershell.exe"], 0, elevated))
        };
        let unelevated = german("False\r\n");
        let arming = prepare(
            &unelevated,
            &cancel,
            Platform::Windows,
            false,
            "linux",
            &target,
        )
        .await
        .unwrap();
        let error = arm(
            &unelevated,
            &cancel,
            Platform::Windows,
            false,
            "linux",
            &arming,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Denied);
        assert_eq!(unelevated.runs().last(), Some(&elevation_probe()));

        // Elevated and still failing: a failure, not a refusal.
        let elevated = german("True\r\n");
        let error = arm(
            &elevated,
            &cancel,
            Platform::Windows,
            false,
            "linux",
            &arming,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Failed);
        assert!(
            error.message.contains("Zugriff verweigert"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn custom_commands_arm_and_must_verify() {
        let target = BootTarget::Commands {
            arm: argv(&["grub-reboot", "Windows"]),
            verify: argv(&["check-next", "Windows"]),
        };
        let cancel = CancellationToken::new();
        let ok = RecordingActuator::new();
        let arming = prepare(&ok, &cancel, Platform::Macos, false, "windows", &target)
            .await
            .unwrap();
        arm(&ok, &cancel, Platform::Macos, false, "windows", &arming)
            .await
            .unwrap();
        assert_eq!(
            ok.runs(),
            [
                argv(&["grub-reboot", "Windows"]),
                argv(&["check-next", "Windows"])
            ]
        );

        let unverified = RecordingActuator::new().reply(Reply::new(&["check-next"], 1, ""));
        let error = arm(
            &unverified,
            &cancel,
            Platform::Linux,
            false,
            "windows",
            &arming,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Failed);
        assert!(
            error.message.contains("check-next Windows"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn macos_cannot_arm_a_firmware_label() {
        let target = BootTarget::EfiLabel("Windows Boot Manager".to_owned());
        let fake = RecordingActuator::new();
        let error = prepare(
            &fake,
            &CancellationToken::new(),
            Platform::Macos,
            false,
            "windows",
            &target,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Unsupported);
        assert!(fake.runs().is_empty());
        assert!(describe(Platform::Macos, &target).starts_with("nothing"));
        assert!(describe(Platform::Linux, &target).contains("efibootmgr"));
        assert!(describe(Platform::Windows, &target).contains("bcdedit"));
    }
}
