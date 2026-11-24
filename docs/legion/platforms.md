# Legion on every platform

This page is the design for reaching Legion from phones, tablets, desktops and browsers, and for
installing it properly on each operating system. It builds on [design.md](design.md) and changes
nothing in it: a tool is still a tool, a machine is still anything that gives you a pipe to
`legion`, and permissions are still checked where the work happens.

## The idea

There are two kinds of participant.

- A **node** runs the `legion` binary and owns tools, policy and an audit log. Nodes are
  computers: Linux, macOS, Windows.
- A **controller** runs no tools. It shows what nodes offer and asks them to do things. Phones and
  tablets are always controllers; the desktop app and the browser are controllers too.

Every controller shows the same interface and speaks the same wire protocol. Only the pipe
differs:

| Controller | Pipe to a node | Who it acts as |
| --- | --- | --- |
| Android, iOS, iPadOS app | ssh from inside the app to `legion rpc` on a paired node | the principal its key is pinned to by `legion gate` |
| Desktop app (macOS, Windows, Linux) | a child process, `legion rpc`, on the same machine | the local owner |
| Browser, or the interface added to a home screen | HTTPS to `legion serve` | the principal of its bearer token |

So there is one interface, one protocol, three pipes, and nothing new to keep running: the phone
needs sshd and `legion` on one machine, which every node already has. Through that one machine it
reaches the rest with `on`, and both the machine it talks to and the machine that does the work
authorise and record the call, as with any forwarded call.

```text
            apps/ui  (one interface, static files, no build step)
                              |
                       Link interface
        hello . tools . call . cancel . events
         /               |                \
   ssh (in-app)     child process       HTTPS
  legion gate        legion rpc       legion serve
         \               |                /
                 a node's tool registry
                         |  on = ...
                  the rest of the fleet
```

## Parts

```text
crates/legion-link     the client side of the wire: sessions over any pipe, the in-app ssh pipe,
                       device keys, invites. No tools, no policy, nothing from legion-os.
apps/ui                the interface: plain ES modules and CSS, served as they are
apps/legion-app        the Tauri 2 shell for Android, iOS, iPadOS, macOS, Windows and Linux
packaging              Debian/Ubuntu and Arch packages of the node, and the systemd user unit
```

`legion-link` takes the session code out of `legion-net` (which then uses it), so the node and the
apps cannot drift apart. It must build for `aarch64-linux-android`, `x86_64-linux-android`,
`aarch64-apple-ios` and `aarch64-apple-ios-sim` as well as every desktop target. Its ssh pipe is a
feature (`ssh`, the `russh` crate) that the `legion` binary does not turn on: nodes keep using the
system `ssh` and the user's own ssh configuration.

`apps/ui` has no build step and no package manager. It is embedded in `legion serve` and bundled
into the app exactly as it sits in the repository. It may vendor small, pinned, checksummed
modules with their licences; it fetches nothing at run time and sets a strict content security
policy. Everything a node returns is shown as text, never as markup.

`apps/legion-app` is its own Cargo workspace, so the node's workspace never builds a web view. Its
Rust side is thin: commands that open links and pass calls through `legion-link`.

## The interface is generic first

Every tool describes itself with a JSON Schema and an effect, so the interface can run any tool
without knowing it: it lists tools, builds a form from the schema, and shows the result. A tool
added tomorrow, or a local extension in `tools.d/`, appears in every app without a new release.

On top of that sit a few made-to-measure views for what people do daily, each only a nicer face
on the same calls: machines at a glance, one machine (state, run a command, jobs, files,
services, projects, power), and this device (who it acts as, what it may do, its links).

Calls whose effect is `destroy` or `power` always ask first and say exactly what will happen;
power calls offer the dry run before the real thing. What the role does not allow is shown as
unavailable rather than hidden, with the reason.

## Pairing a phone or tablet

A device pairs with a node once. The device makes its own ed25519 key and the private half never
leaves it.

1. On the node: `legion pair` (the tool `device.invite`). It makes a single-use
   temporary key, adds it to `authorized_keys` with a forced command that can only enrol a device
   and with a portable 12-digit `expiry-time` bound (compatible with OpenSSH 8.9+), and prints an
   invite as a QR code and as text. The invite carries where to connect (the Tailscale name when
   there is one), the account, the node's ssh host keys (when `[devices] host_keys` is unset,
   intersecting `ssh-keyscan` with readable host `.pub` files and refusing disjoint results), and
   the temporary key. The invite text is a secret until used: run `legion pair` in your own terminal
   rather than letting an AI agent call `device.invite`, which would place the invite in the agent's
   transcript.
2. In the app: scan or paste the invite. The app connects, checks the host key against the
   invite (there is no trust on first use), and hands over its public key and a name.
3. The node enforces exact UTC expiry against `state/devices.json` (and refuses the temporary key
   or any key already present in `authorized_keys` or `devices.toml`), records the device in
   `devices.toml` beside `policy.toml`, reconciles `authorized_keys` so the temporary line is
   replaced with a permanent one that forces
   `legion gate --as <principal> --device <name> --pairing <id>` (always pinning a random
   per-pairing id and `--home` or `--config-dir`/`--state-dir`, plus `--account` on Windows's
   shared `administrators_authorized_keys`), pushes `devices.toml` to reachable peers in
   `fleet.toml`, and audits it with `door: "ssh"`. From then on the device can only speak the
   Legion protocol as that principal, with that principal's role, and every call it makes is
   reported and audited with `door: "ssh"` and its device name.

`device.list` shows paired devices (reconciling `authorized_keys` from `devices.toml`, adopting
pre-existing `--pairing` lines on first run, sweeping expired invites recorded by this home, and
reporting foreign or orphaned Legion lines for owner callers), `device.set` changes a paired
device's role or name and pushes the update to the fleet, `device.endpoint` (`effect: read`)
advertises a node's direct SSH connection details and pinned host keys, and `device.remove`
tombstones the device in `devices.toml`, removes its key and unused `device:*` principal,
immediately terminates any live sessions and verified same-account parent `sshd` connections for
that device, reports any background jobs the device left running in `jobs_left_running` without
killing them, and pushes the revocation to reachable peers (naming unreachable peers in
`still_trusted_by`). Making an invite or changing a role needs the `admin` effect and follows the
token rule: only for a principal the caller could act as (and `owner` role for creating
`device:<name>` principals or changing roles), and never via a forwarded or local expiring
credential.

The key is kept in the app's private storage, like the earlier Android app did. Wrapping it with
the platform's hardware keystore is a later step.

## A device belongs to the fleet, not to one machine

Nothing is central. Any node can pair a device, any node can serve one, and a device keeps working
when the node that paired it is switched off. Three rules make that true.

**The role is chosen when pairing.** `legion pair` asks what the device may do, or takes
`--role <role>`: any role the node's policy defines, from looking only to the owner role. It makes
a principal of its own for the device (`device:<name>`), so two devices never share an identity,
and `device.set` changes the role later. A device paired as owner can do everything any computer
can, including pairing further devices. `--as <principal>` still pairs under an existing principal.
No role and no device name is built in; what a person prefers lives in their own policy.

**Paired devices are fleet configuration.** They are kept in `devices.toml` beside `policy.toml`:
the device's name, public key, principal, recorded role (for `device:*` principals), when and where
it was paired and updated, and, once removed, a tombstone. `fleet.sync` carries the file like the
policy, but merges it in both directions instead of replacing it: entries are added from both sides
and a tombstone always wins, so two nodes that each paired or removed something while apart agree
again when they meet (and `meta.doctor` warns if any reachable peer's `devices.toml` differs).
Every node adopts any pre-existing `--pairing` lines on first run, keeps its own `authorized_keys`
and `device:*` principals in step with the file whenever it pairs, syncs, lists, updates, or
removes (failing closed to `nobody` if a synced role is undefined locally), and the gate refuses a
device the file no longer names (ending its parent `sshd` on `Denied`). Pairing, updating, and
removing push in parallel to every node that can be reached at that moment and say which ones could
not; those catch up at their next sync (and immediately tear down active sessions for any newly
tombstoned devices on sync). Until then a removed device is still accepted by a node that has not
heard: that is the price of having no hub, and `device.remove` says so by name in
`still_trusted_by`.

**The device learns every way in.** Each node can describe how a device reaches it (`device.endpoint`:
names, port, account, ssh host keys). After pairing, and from then on in the background, the app
asks for that from every machine through whichever node answers, and keeps it. It then talks to
each machine directly where it can and through another node where it cannot, and shows which.
A machine added to the fleet later appears in the app by itself once the fleet has synced.

What a device is told about a machine comes from a node it already trusts; the host keys it pins
for a new machine are the ones that node reports. A node with the owner role over the fleet could
lie about them, and could equally do anything else.

## The browser door

`legion serve` serves the interface at `/` beside `/v1/*`. It is still loopback only and still
published only by something like Tailscale Serve. The page asks for a bearer token once and keeps
it for that origin. With a web manifest and icons it can be added to a home screen on iOS, iPadOS
and Android, which is the way onto an iPhone or iPad without an Apple developer account.

## Installing

| System | Node | App |
| --- | --- | --- |
| Debian 12 and 13, Ubuntu 22.04, 24.04 and 26.04 | `.deb` for amd64 and arm64 around the static binary, with a systemd user unit for `legion serve` | `.deb` and AppImage |
| Arch | `PKGBUILD` (from source, and a `-bin` variant from a release) with the same unit | `PKGBUILD` |
| macOS | `install/legion.sh` | `.dmg` |
| Windows | `install/legion.ps1` | installer `.exe` |
| Android | none: a controller | `.apk`, arm64 |
| iOS, iPadOS | none: a controller | the app (needs Apple signing to reach a real device), the browser door meanwhile |

Packages are named `legion` (and `legion-bin` for the prebuilt Arch binary). They never start
anything or change configuration on install: `legion init` stays the first step, and the user unit
is enabled by the user. When installed from a package, `meta.update` and `fleet.install` refuse
in-place replacement and point at `apt` or `pacman` instead.

## How it is verified

- Each Linux distribution above installs its package in a clean container, and then runs the
  real-ssh end-to-end script against it, on every change.
- The Android app is built as a universal debug APK, installed and launched in a KVM emulator in CI
  until the interface reports ready (capturing dark and light screenshots); the full end-to-end
  pairing and SSH test (`apps/legion-app/e2e-android.sh`) runs against a real node and loopback
  `sshd` locally (and can move into CI once `legion pair` merges to `main`).
- The iOS app is built for the simulator, installed and launched on a current iPhone and iPad
  simulator in CI until the interface reports ready (verifying iCloud backup exclusion on the app
  data directory and capturing dark and light screenshots on both), alongside an unsigned device
  `.ipa`.
- The desktop app is bundled on Linux (`.deb` and AppImage), macOS (ad-hoc signed Apple Silicon
  `.app` and `.dmg`) and Windows (NSIS installer `.exe`), started once on each runner (silently
  installed first on Windows, under Xvfb on Linux), and verified to render until the interface
  reports ready.
- `legion-link` is cross-checked for the Android and iOS targets in the Rust job.

## Trade-offs taken

- **A web view, not three native interfaces.** One interface for six platforms and the browser,
  kept by one person and their agents, beats native polish here. The interface is written to feel
  at home on a phone first.
- **ssh from the phone, not a server for the phone.** It keeps the promise that nothing has to
  stay running, and a paired phone is confined by the same gate as any restricted key. The
  browser door exists for devices that cannot run the app.
- **Phones do not run tools.** A phone is not a machine to manage here. Android could host a node
  later; iOS cannot.
