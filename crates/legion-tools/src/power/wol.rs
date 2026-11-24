//! Wake-on-LAN: the magic packet, and where on this machine's networks to send it.
//!
//! A magic packet is a broadcast, so it reaches only the network segment it is sent on. It goes to
//! the broadcast address of every interface that has one and is connected, and to
//! 255.255.255.255, which an operating system sends out of one interface only. UDP is fire and
//! forget and a sleeping network card gets one chance to notice, so each goes out a few times.
//! A packet sent is never a machine woken: only the machine answering says that.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use legion_core::CancellationToken;
use legion_core::proto::{ErrorBody, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::actuator::Actuator;
use super::plan::{Platform, strings};

/// The port wake packets go to: the discard port, which nothing listens on.
pub const WOL_PORT: u16 = 9;
/// How many times each packet goes to each address.
pub const REPEATS: usize = 3;
/// How long listing the interfaces may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// Reads a hardware address written `02:00:00:00:00:01` or `02-00-00-00-00-01`.
pub fn parse_mac(text: &str) -> Result<[u8; 6], String> {
    let parts: Vec<&str> = text.split([':', '-']).collect();
    let mut mac = [0u8; 6];
    if parts.len() != 6 {
        return Err(format!(
            "{text:?} is not a hardware address: write six hex pairs such as 02:00:00:00:00:01"
        ));
    }
    for (byte, part) in mac.iter_mut().zip(&parts) {
        if part.len() != 2 {
            return Err(format!(
                "{text:?} is not a hardware address: every part is two hex digits"
            ));
        }
        *byte = u8::from_str_radix(part, 16).map_err(|_| {
            format!("{text:?} is not a hardware address: {part:?} is not hexadecimal")
        })?;
    }
    Ok(mac)
}

/// The address as `02:00:00:00:00:01`.
pub fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The 102-byte magic packet: six `0xff` bytes, then the address sixteen times.
pub fn magic_packet(mac: [u8; 6]) -> Vec<u8> {
    let mut packet = vec![0xff; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    packet
}

/// An interface a wake packet can be broadcast on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Interface {
    /// Its name: `eth0`, `en0`, `Ethernet`.
    pub name: String,
    /// This machine's address on it.
    pub address: Ipv4Addr,
    /// The network's broadcast address.
    pub broadcast: Ipv4Addr,
}

/// The read-only command that lists this platform's IPv4 interfaces.
pub fn interfaces_command(platform: Platform) -> Vec<String> {
    match platform {
        Platform::Linux => strings(&["ip", "-j", "-4", "addr", "show", "up"]),
        Platform::Macos => strings(&["ifconfig"]),
        Platform::Windows => strings(&[
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "ConvertTo-Json -Compress -InputObject @(Get-NetIPAddress -AddressFamily IPv4 -AddressState Preferred -ErrorAction Stop | Select-Object InterfaceAlias, IPAddress, PrefixLength)",
        ]),
    }
}

/// Reads the interfaces from what [`interfaces_command`] printed.
pub fn parse_interfaces(platform: Platform, text: &str) -> Vec<Interface> {
    match platform {
        Platform::Linux => parse_ip_json(text),
        Platform::Macos => parse_ifconfig(text),
        Platform::Windows => parse_net_ip_address(text),
    }
}

/// `ip -j -4 addr show up`: connected (`LOWER_UP`) broadcast interfaces, loopback left out.
pub fn parse_ip_json(text: &str) -> Vec<Interface> {
    #[derive(Deserialize)]
    struct Link {
        ifname: String,
        #[serde(default)]
        flags: Vec<String>,
        #[serde(default)]
        addr_info: Vec<Address>,
    }
    #[derive(Deserialize)]
    struct Address {
        #[serde(default)]
        family: String,
        #[serde(default)]
        local: Option<Ipv4Addr>,
        #[serde(default)]
        broadcast: Option<Ipv4Addr>,
    }
    let links: Vec<Link> = serde_json::from_str(text).unwrap_or_default();
    let mut found = Vec::new();
    for link in links {
        let has = |flag: &str| link.flags.iter().any(|f| f == flag);
        if has("LOOPBACK") || !has("BROADCAST") || !has("LOWER_UP") {
            continue;
        }
        for address in link.addr_info {
            if let (true, Some(local), Some(broadcast)) =
                (address.family == "inet", address.local, address.broadcast)
            {
                found.push(Interface {
                    name: link.ifname.clone(),
                    address: local,
                    broadcast,
                });
            }
        }
    }
    found
}

/// `ifconfig` on macOS: interfaces that are up, running and broadcast, loopback left out.
pub fn parse_ifconfig(text: &str) -> Vec<Interface> {
    let mut found = Vec::new();
    let mut current: Option<(String, bool)> = None;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            current = line.split_once(": flags=").map(|(name, rest)| {
                let flags = rest
                    .split_once('<')
                    .and_then(|(_, rest)| rest.split_once('>'))
                    .map_or("", |(flags, _)| flags);
                let has = |flag: &str| flags.split(',').any(|f| f == flag);
                let usable = has("UP") && has("RUNNING") && has("BROADCAST") && !has("LOOPBACK");
                (name.to_owned(), usable)
            });
            continue;
        }
        let Some((name, true)) = &current else {
            continue;
        };
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.first() != Some(&"inet") {
            continue;
        }
        let address = words.get(1).and_then(|word| word.parse().ok());
        let broadcast = words
            .iter()
            .position(|word| *word == "broadcast")
            .and_then(|at| words.get(at + 1))
            .and_then(|word| word.parse().ok());
        if let (Some(address), Some(broadcast)) = (address, broadcast) {
            found.push(Interface {
                name: name.clone(),
                address,
                broadcast,
            });
        }
    }
    found
}

/// `Get-NetIPAddress` as JSON on Windows: each address with its network's broadcast worked out
/// from the prefix; loopback, link-local and point-to-point (/31, /32) addresses left out.
pub fn parse_net_ip_address(text: &str) -> Vec<Interface> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Row {
        interface_alias: String,
        #[serde(rename = "IPAddress")]
        ip_address: Ipv4Addr,
        prefix_length: u8,
    }
    let rows: Vec<Row> = match serde_json::from_str::<Value>(text.trim()) {
        Ok(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| serde_json::from_value(item).ok())
            .collect(),
        Ok(item @ Value::Object(_)) => serde_json::from_value(item).into_iter().collect(),
        _ => Vec::new(),
    };
    rows.into_iter()
        .filter(|row| {
            !row.ip_address.is_loopback()
                && !row.ip_address.is_link_local()
                && (1..31).contains(&row.prefix_length)
        })
        .map(|row| {
            let host_bits = u32::MAX >> row.prefix_length;
            Interface {
                name: row.interface_alias,
                address: row.ip_address,
                broadcast: Ipv4Addr::from(u32::from(row.ip_address) | host_bits),
            }
        })
        .collect()
}

/// The addresses a wake packet goes to: each interface's broadcast, then 255.255.255.255.
pub fn destinations(interfaces: &[Interface]) -> Vec<SocketAddrV4> {
    let mut targets: Vec<SocketAddrV4> = Vec::new();
    for address in interfaces
        .iter()
        .map(|interface| interface.broadcast)
        .chain([Ipv4Addr::BROADCAST])
    {
        let target = SocketAddrV4::new(address, WOL_PORT);
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

/// What sending wake packets came to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Sent {
    /// The hardware address the packets wake.
    pub mac: String,
    /// How many datagrams left this machine; 0 on a dry run.
    pub packets: usize,
    /// The interfaces whose networks they were broadcast on.
    pub interfaces: Vec<Interface>,
    /// Every `address:port` they went to, each [`REPEATS`] times.
    pub destinations: Vec<String>,
    /// What went wrong along the way without stopping every packet.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Sends the magic packet for `mac` to every destination, [`REPEATS`] times each; with `dry_run`,
/// only says where it would go. Fails when not one datagram could be sent.
pub async fn send(
    actuator: &dyn Actuator,
    cancel: &CancellationToken,
    platform: Platform,
    mac: &str,
    dry_run: bool,
) -> Result<Sent, ErrorBody> {
    let bytes = parse_mac(mac).map_err(|reason| {
        ErrorBody::new(ErrorCode::InvalidArgs, format!("{reason}."))
            .with_hint("Fix the mac setting of the machine in fleet.toml.")
    })?;
    let mut warnings = Vec::new();
    let listed = actuator
        .run(&interfaces_command(platform), LIST_TIMEOUT, cancel)
        .await;
    let interfaces = match listed {
        Ok(ran) if ran.ok() => parse_interfaces(platform, &ran.stdout),
        Ok(ran) => {
            warnings.push(format!(
                "this machine's interfaces could not be listed ({}), so the packet went to 255.255.255.255 only",
                ran.first_line()
            ));
            Vec::new()
        }
        Err(error) => {
            warnings.push(format!(
                "this machine's interfaces could not be listed ({}), so the packet went to 255.255.255.255 only",
                error.message
            ));
            Vec::new()
        }
    };
    let targets = destinations(&interfaces);
    let packet = magic_packet(bytes);
    let mut packets = 0;
    let mut refused = Vec::new();
    if !dry_run {
        for target in &targets {
            for _ in 0..REPEATS {
                match actuator.send_datagram(&packet, *target) {
                    Ok(()) => packets += 1,
                    Err(reason) => {
                        refused.push(format!("{target}: {reason}"));
                        break;
                    }
                }
            }
        }
        if packets == 0 {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "No wake packet for {} left this machine ({}).",
                    format_mac(bytes),
                    refused.join("; ")
                ),
            )
            .with_hint(
                "Check that this machine is connected to the network the sleeping machine is on.",
            )
            .with_details(json!({"refused": refused})));
        }
        warnings.extend(
            refused
                .into_iter()
                .map(|reason| format!("not sent to {reason}")),
        );
    }
    Ok(Sent {
        mac: format_mac(bytes),
        packets,
        interfaces,
        destinations: targets.iter().map(ToString::to_string).collect(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::fake::{Record, RecordingActuator, Reply};

    #[test]
    fn the_magic_packet_for_a_known_address() {
        let mac = parse_mac("02:00:00:00:00:01").unwrap();
        assert_eq!(mac, [0x02, 0, 0, 0, 0, 0x01]);
        assert_eq!(parse_mac("02-00-00-00-00-0A").unwrap()[5], 0x0a);
        let packet = magic_packet(mac);
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[..6], &[0xff; 6]);
        for repeat in packet[6..].chunks(6) {
            assert_eq!(repeat, &[0x02, 0, 0, 0, 0, 0x01]);
        }
        assert_eq!(format_mac(mac), "02:00:00:00:00:01");
        for wrong in [
            "02:00:00:00:00",
            "02:00:00:00:00:0g",
            "0200.0000.0001",
            "2:0:0:0:0:1",
            "",
        ] {
            assert!(parse_mac(wrong).is_err(), "{wrong}");
        }
    }

    const IP_JSON: &str = r#"[
{"ifindex":1,"ifname":"lo","flags":["LOOPBACK","UP","LOWER_UP"],"addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8,"scope":"host"}]},
{"ifindex":2,"ifname":"eth0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"addr_info":[{"family":"inet","local":"192.0.2.10","prefixlen":25,"broadcast":"192.0.2.127","scope":"global"}]},
{"ifindex":3,"ifname":"tailscale0","flags":["POINTOPOINT","MULTICAST","NOARP","UP","LOWER_UP"],"addr_info":[{"family":"inet","local":"192.0.2.200","prefixlen":32,"scope":"global"}]},
{"ifindex":4,"ifname":"docker0","flags":["NO-CARRIER","BROADCAST","MULTICAST","UP"],"addr_info":[{"family":"inet","local":"192.0.2.129","prefixlen":26,"broadcast":"192.0.2.191","scope":"global"}]},
{"ifindex":5,"ifname":"wlan0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"addr_info":[{"family":"inet","local":"192.0.2.140","prefixlen":26,"broadcast":"192.0.2.191","scope":"global"}]}
]"#;

    #[test]
    fn linux_interfaces_are_the_connected_broadcast_ones() {
        let found = parse_ip_json(IP_JSON);
        let names: Vec<(&str, String)> = found
            .iter()
            .map(|i| (i.name.as_str(), i.broadcast.to_string()))
            .collect();
        assert_eq!(
            names,
            [
                ("eth0", "192.0.2.127".to_owned()),
                ("wlan0", "192.0.2.191".to_owned())
            ]
        );
        assert!(parse_ip_json("not json").is_empty());
    }

    #[test]
    fn macos_interfaces_come_from_ifconfig() {
        let text = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n\
\tinet 127.0.0.1 netmask 0xff000000\n\
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\
\tether 02:00:00:00:00:05\n\
\tinet6 fe80::1%en0 prefixlen 64 secured scopeid 0xb\n\
\tinet 192.0.2.20 netmask 0xffffff80 broadcast 192.0.2.127\n\
\tstatus: active\n\
en1: flags=8822<BROADCAST,SMART,SIMPLEX,MULTICAST> mtu 1500\n\
\tinet 192.0.2.150 netmask 0xffffff80 broadcast 192.0.2.255\n\
utun3: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1280\n\
\tinet 192.0.2.50 --> 192.0.2.50 netmask 0xffffffff\n";
        assert_eq!(
            parse_ifconfig(text),
            [Interface {
                name: "en0".to_owned(),
                address: "192.0.2.20".parse().unwrap(),
                broadcast: "192.0.2.127".parse().unwrap(),
            }]
        );
    }

    #[test]
    fn windows_interfaces_work_out_their_broadcast() {
        let many = r#"[{"InterfaceAlias":"Ethernet","IPAddress":"192.0.2.30","PrefixLength":24},{"InterfaceAlias":"Loopback Pseudo-Interface 1","IPAddress":"127.0.0.1","PrefixLength":8},{"InterfaceAlias":"Tailscale","IPAddress":"192.0.2.201","PrefixLength":32},{"InterfaceAlias":"Wi-Fi","IPAddress":"169.254.10.20","PrefixLength":16}]"#;
        let found = parse_net_ip_address(many);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Ethernet");
        assert_eq!(found[0].broadcast, Ipv4Addr::new(192, 0, 2, 255));
        let one = r#"{"InterfaceAlias":"Ethernet 2","IPAddress":"192.0.2.70","PrefixLength":26}"#;
        assert_eq!(
            parse_net_ip_address(one)[0].broadcast,
            Ipv4Addr::new(192, 0, 2, 127)
        );
    }

    #[test]
    fn destinations_are_each_broadcast_once_then_the_limited_broadcast() {
        let found = parse_ip_json(IP_JSON);
        let targets: Vec<String> = destinations(&found)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            targets,
            ["192.0.2.127:9", "192.0.2.191:9", "255.255.255.255:9"]
        );
        assert_eq!(destinations(&[]).len(), 1);
    }

    #[tokio::test]
    async fn sending_repeats_each_packet_and_counts_what_left() {
        let fake = RecordingActuator::new().reply(Reply::new(&["ip"], 0, IP_JSON));
        let cancel = CancellationToken::new();
        let sent = send(&fake, &cancel, Platform::Linux, "02:00:00:00:00:01", false)
            .await
            .unwrap();
        assert_eq!(sent.packets, 3 * REPEATS);
        assert_eq!(sent.mac, "02:00:00:00:00:01");
        assert_eq!(sent.interfaces.len(), 2);
        let expected_hex: String = magic_packet([2, 0, 0, 0, 0, 1])
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let datagrams: Vec<(String, String)> = fake
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Datagram { to, hex } => Some((to, hex)),
                _ => None,
            })
            .collect();
        assert_eq!(datagrams.len(), 9);
        assert!(datagrams.iter().all(|(_, hex)| *hex == expected_hex));
        assert_eq!(datagrams[8].0, "255.255.255.255:9");
    }

    #[tokio::test]
    async fn a_dry_run_sends_nothing_and_a_failed_listing_still_broadcasts() {
        let cancel = CancellationToken::new();
        let fake = RecordingActuator::new().reply(Reply::new(&["ip"], 0, IP_JSON));
        let dry = send(&fake, &cancel, Platform::Linux, "02:00:00:00:00:01", true)
            .await
            .unwrap();
        assert_eq!(dry.packets, 0);
        assert_eq!(dry.destinations.len(), 3);
        assert_eq!(fake.records().len(), 1);

        let blind = RecordingActuator::new().reply(Reply::new(&["ifconfig"], 1, "boom"));
        let sent = send(&blind, &cancel, Platform::Macos, "02:00:00:00:00:01", false)
            .await
            .unwrap();
        assert_eq!(sent.packets, REPEATS);
        assert_eq!(sent.destinations, ["255.255.255.255:9"]);
        assert!(sent.warnings[0].contains("could not be listed"));
    }

    #[tokio::test]
    async fn not_one_packet_sent_is_a_failure() {
        let fake = RecordingActuator::new().refusing_datagrams();
        let error = send(
            &fake,
            &CancellationToken::new(),
            Platform::Windows,
            "02:00:00:00:00:01",
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Failed);
        assert!(
            error.message.contains("No wake packet"),
            "{}",
            error.message
        );
        let bad = send(
            &RecordingActuator::new(),
            &CancellationToken::new(),
            Platform::Linux,
            "02:00:00",
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(bad.code, ErrorCode::InvalidArgs);
    }
}
