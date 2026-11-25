# Updates: staying current without downloading anything by hand

Legion is several programs on several kinds of machine: the node (`legion`), the app on phones,
tablets and desktops, and the interface the HTTP door serves. This is how updates work today, with
one rule for what may be installed and no new place that has to be trusted.

It builds on what `meta.update` already does for a node: a release is a set of files listed in
`SHA256SUMS`, and `SHA256SUMS.sig` is an Ed25519 signature over those exact bytes by a key whose
public half is compiled into the program.

## The rule

A program installs a newer copy of itself only when all of these hold:

1. The set's `SHA256SUMS` is signed by a key the running program was built to trust.
2. The file it is about to install is listed there with an exact byte length and SHA-256 digest, and
   both match on disk before anything executes.
3. The set is strictly newer than what is running and strictly newer than the device's monotonic
   sequence floor (`floor.json`), by the set's own sequence number, which the signature covers.
4. The sequence is within the plausibility horizon (no more than 10 years / 3,650 days ahead of the
   running build's calendar sequence, preventing a stray far-future sequence from locking out future
   updates).
5. The file is the unique artifact for this platform, architecture, and install kind.
6. The running build has a non-zero build sequence (`LEGION_BUILD_SEQUENCE > 0`). Sequence-0
   development builds disable self-updates completely so they can never advance or corrupt a
   device's downgrade floor.

There is no flag that skips rules 1 to 6. What differs between a release and a preview is only which
key signed the set.

## An update set

A directory containing:

- `update.json`: `{channel, version, build, sequence, published_at, notes, artifacts}`.
  `sequence` is a whole number that only grows (the build time in UTC as `YYYYMMDDHHMM`).
  `notes` is a few plain sentences a person reads before or after updating. Each artifact entry is
  `{name, program: "node" | "app", os, arch, kind, size}`, where `kind` is how it installs:
  `binary`, `deb`, `apk`, `appimage`, `app-tar` (a macOS `.app` bundle tarred and gzipped), or
  `nsis`, and `size` is the signed exact byte length of the artifact.
- the artifacts themselves,
- `SHA256SUMS` listing `update.json` and every artifact, and `SHA256SUMS.sig`.

`scripts/make-update-set.mjs create` refuses sequences more than 24 hours ahead of UTC now and
inspects each artifact's embedded sequence or version metadata (APK binary AXML `versionCode`,
macOS `Info.plist` `CFBundleVersion`, `.deb` `Version` / binary marker, NSIS `+seq<N>` version
string, AppImage SquashFS marker, and node binary `LEGION_UPDATE_SEQUENCE=<N>` marker) without
executing any artifact, refusing to sign if the embedded sequence does not match `--sequence`.

Crate `legion-update` holds what both the node and the app need: parsing and checking a set,
strict Ed25519 SPKI public key parsing, sequence floor recovery, sequence plausibility checks, and
unambiguous artifact selection.

### Node GitHub releases and `SHA256SUMS`

Node releases published by `.github/workflows/legion-release.yml` carry `SHA256SUMS` and
`SHA256SUMS.sig` signed with the offline release key, and do **not** include `update.json`. That
separation is intentional and load-bearing: `meta.update` verifies raw GitHub release `SHA256SUMS`
against `contract/release-public-key.pem` by release tag and asset name, whereas fleet update sets
always require `update.json` listed inside `SHA256SUMS` so a raw node release `SHA256SUMS` can
never be replayed as a fleet update set (and vice versa).

## Channels and keys

- `stable`: signed offline with the release key (`contract/release-public-key.pem`), published as
  a GitHub release. Every build trusts it.
- `preview`: signed with a preview key that lives on the machine that makes preview builds. A
  build trusts it only when it was built with that key's public half
  (`LEGION_PREVIEW_UPDATE_KEY` at build time). Official release and CI builds leave it unset and can
  never install a preview. A preview build still takes a stable release when that is newer.

Which channel a running program follows is shown under Technical details and is otherwise implied by
how it was built.

## Where sets come from

**From the fleet (implemented).** A node keeps the update sets it has been given in its state
directory (`updates/<channel>/<sequence>/`, keeping the newest two per channel). Any paired device
or fleet machine may read them; they are not secret, and the reader checks the signature itself.

- `update.list` (read): lists the sets this node holds, newest first, with channel, sequence, and
  artifact metadata.
- `update.read` (read): `{channel, sequence, name, offset, length}` returns up to 1 MiB of one
  listed file as base64, like `fs.read`. Only `SHA256SUMS`, `SHA256SUMS.sig`, and bare filenames
  listed in that set's `SHA256SUMS` exist.
- `update.publish` (admin): `{from}` verifies a directory on this node against the keys this node
  trusts, stores it atomically, and notifies reachable fleet peers with node authority so each peer
  pulls, verifies, and stores the set for itself. An offline peer can pull it later with
  `update.publish from_peer=<peer> channel=<channel> sequence=<seq>`.

When checking for updates, the app queries every reachable paired machine, caps candidates and
metadata bytes per machine, enforces overall deadlines and minimum download progress rates, and
automatically falls back to another paired machine if one machine serves corrupted data, wrong
offsets, truncated reads, or stalls. Manual checks and manual installs immediately cancel any
in-flight background check.

On metered connections (reported on Android via `ConnectivityManager`), automatic background
downloads pause when an artifact is larger than 5 MiB; tapping **Update now** downloads it anyway.

**From GitHub (current vs planned).** For standalone `legion` nodes, `meta.update` already fetches
and verifies `stable` releases from GitHub over HTTPS. Direct GitHub release polling from the app
and scheduled daily node update timers are **planned** and not yet implemented.

## How each kind of install updates

| Install | Current behaviour |
| --- | --- |
| Android app (APK) | Downloads the verified APK from a paired machine and installs it through Android's `PackageInstaller`. Android enforces that the new APK is signed with the same app signing key and carries a higher `versionCode`. The first time, Android opens **Install unknown apps** once so the person allows Legion to install updates. On Android 12 and later, Legion declares `UPDATE_PACKAGES_WITHOUT_USER_ACTION` and requests `USER_ACTION_NOT_REQUIRED`, so Android can apply updates without prompting when no competing store owns the package and Android's 30-second silent-update throttle is not active; otherwise Android shows its standard confirmation dialog (and background updates defer silently without bringing Legion forward). Unlike desktop swaps, Android replaces the APK atomically via `PackageInstaller` rather than keeping a separate `.prev` copy on disk; if installation fails or the next start does not match the expected sequence, Legion records the failure and stops auto-retrying that sequence until a newer one arrives or the person taps **Try again**. |
| macOS app (`.app`) | Copies-while-hashing the verified `app-tar` into a private `0700` directory, unpacks it safely without following symlinks or preserving setuid/setgid bits, swaps the bundle atomically while keeping `.prev`, and restarts under a detached minimal-environment supervisor. If the new copy does not confirm startup, the supervisor restores `.prev`, relaunches it, and records a rollback notice. |
| Windows app (NSIS) | Copies-while-hashing the verified per-user NSIS installer into a private folder under `%LOCALAPPDATA%`, backs up the existing install directory, and runs a detached System32 PowerShell supervisor (`-EncodedCommand`, breaking out of the parent job object when permitted) that executes `/S /UPDATE /D=<install_dir>` and verifies startup. If the installer exits non-zero or the new build fails to start, the supervisor restores the backup and records a rollback notice. |
| Linux app (AppImage) | Copies-while-hashing the verified AppImage beside the running file (`0755`), swaps atomically with `.prev` under a detached `/bin/sh` supervisor with a cleared environment (stripping all `$APPDIR` and temporary paths and resolving system utilities from fixed directories), and rolls back automatically if startup is not confirmed. |
| Linux app from `.deb` | Never replaces files `dpkg` owns. Downloads and verifies the `.deb` into a private `0700` folder, displays the verified SHA-256 digest and the exact `sudo apt-get install …` command to run. |
| Linux app from `pacman` or standalone build | Checks for updates and reports when a newer version is available, without downloading artifacts it cannot install. |
| Node (`legion`), standalone | `meta.update`, with its existing preflight, `.prev` backup and automatic restore. Automatic node self-update (`[update] auto` in `node.toml`) and a daily GitHub timer unit are **planned** and not yet implemented. |
| iPhone and iPad app | Cannot update itself; Apple allows updates only through the App Store or TestFlight. |
| Browser | Always as new as the node that serves it. |

Desktop self-replacing installs (Linux AppImage, macOS `.app`, Windows NSIS) keep the previous copy
until the new build starts once and writes its startup marker, restoring the previous copy if it
fails. If a device's `floor.json` becomes damaged or unreadable, Legion rebuilds the floor at the
running build's sequence and surfaces the recovery in Settings > About (with raw error details under
Technical details).

## What a person sees

Settings > About shows **Updates**:

- One plain line of state: "Up to date", "Version 0.2.0 is ready", "Downloading…", "Restart to
  finish", "Updates disabled in this build" (for sequence-0 dev builds), or a plain sentence when
  something needs attention (raw node or OS diagnostics stay under Technical details).
- One action button when there is something to do: **Update now** (or **Try again** after a failed
  attempt). Tapping **Update now** while a command, interactive call, or file transfer is running
  asks before restarting.
- **Update automatically** (on by default on self-updating installs): downloads when a newer version
  is seen (pausing downloads over 5 MiB on metered connections) and installs only at two safe
  moments — at app start before the person interacts (showing a brief "Updating…" screen) or when
  leaving the app (closing the last desktop window, which verifies startup silently without opening
  a window, or backgrounding Legion on Android when silent self-update is available). There is no
  idle timer that interrupts an open session.
- **What is new** with the set's notes, and after an update a quiet one-time unbordered row on the
  first screen: `Updated to 0.2.0 · What is new` with a subtle `×` dismiss button.
- With Technical details enabled: channel, build, sequence (`0 (updates disabled)` on sequence-0
  builds), install kind, signer key fingerprint, source machine, checked machines, failed sequence
  info, and any raw diagnostic error detail.

The app checks when it starts, when it returns to the foreground after an hour or more, and every
six hours while open. A check that finds no machine reachable stays quiet.

**Planned UI additions (not yet implemented):** showing an "Update" row on individual machine
screens when a remote node is behind the fleet's newest set.

## Making a preview

`scripts/make-update-set.mjs` verifies each artifact's embedded sequence, records exact byte sizes,
writes `update.json` and `SHA256SUMS`, and signs `SHA256SUMS.sig` with the key it is given.
`legion update.publish from=<dir>` on any machine in the fleet puts it in circulation. A stable
release is the same set attached to a GitHub release and signed with the offline key by
`scripts/sign-legion-release.mjs`.

The Android `versionCode` (derived from the UTC `YYYYMMDDHHMM` sequence as Unix minutes, or small
positive test sequences `1..=20_000_000`) and the desktop bundle versions (`<version>+seq<sequence>`
on Linux and Windows, `CFBundleVersion=<sequence>` on macOS) are derived from the sequence, so each
platform's own downgrade protection agrees with Legion's.
