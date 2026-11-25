# Updates: staying current without downloading anything by hand

Legion is several programs on several kinds of machine: the node (`legion`), the app on phones,
tablets and desktops, and the interface the HTTP door serves. This is how each of them gets
newer, with one rule for what may be installed and no new place that has to be trusted.

It builds on what `meta.update` already does for a node: a release is a set of files listed in
`SHA256SUMS`, and `SHA256SUMS.sig` is an Ed25519 signature over those exact bytes by a key whose
public half is compiled into the program.

## The rule

A program installs a newer copy of itself only when all of these hold:

1. The set's `SHA256SUMS` is signed by a key the running program was built to trust.
2. The file it is about to install is listed there and its SHA-256 matches.
3. The set is newer than what is running (and newer than anything it has run before), by the
   set's own sequence number, which the signature covers.
4. The file is for this platform and this kind of install.

There is no flag that skips 1 to 3. What differs between a release and a preview is only which
key signed the set.

## An update set

A directory:

- `update.json`: `{channel, version, build, sequence, published_at, notes, artifacts}`.
  `sequence` is a whole number that only grows (the build time in UTC as `YYYYMMDDHHMM`).
  `notes` is a few plain sentences a person reads before or after updating. Each artifact is
  `{name, program: "node" | "app", os, arch, kind}`, where `kind` is how it installs: `binary`,
  `deb`, `apk`, `appimage`, `app-tar` (a macOS app bundle packed), `nsis`.
- the artifacts themselves,
- `SHA256SUMS` listing `update.json` and every artifact, and `SHA256SUMS.sig`.

Crate `legion-update` holds what both the node and the app need: parsing and checking a set,
the trusted keys, the ordering, choosing the artifact for a platform. `meta.update`'s existing
verification moves there unchanged in behaviour.

## Channels and keys

- `stable`: signed offline with the release key (`contract/release-public-key.pem`), published as
  a GitHub release. Every build trusts it.
- `preview`: signed with a preview key that lives on the machine that makes preview builds. A
  build trusts it only when it was built with that key's public half
  (`LEGION_PREVIEW_UPDATE_KEY` at build time). Official builds are made without it and can never
  install a preview. A preview build still takes a stable release when that is newer.

Which channel a running program follows is shown under Technical details and is otherwise
implied by how it was built.

## Where sets come from

**From the fleet.** A node keeps the update sets it has been given in its state directory
(`updates/<channel>/<sequence>/`, the newest two per channel). Any paired device or fleet machine
may read them; they are not secret, and the reader checks the signature itself.

- `update.list` (read): the sets this node holds, each with channel, version, build, sequence,
  notes, artifacts, and which trusted key (if any) signed it.
- `update.read` (read): `{channel, sequence, name, offset, length}` returns up to 1 MiB of one
  listed file as base64, like `fs.read`. Only `SHA256SUMS`, `SHA256SUMS.sig`, and names listed in
  that set's `SHA256SUMS` exist.
- `update.publish` (admin): `{from}` takes a directory on this node, checks the set against the
  keys this node trusts, and stores it. Then, with the node's own authority, it tells the fleet
  machines it can reach, each of which pulls the set, checks it and stores it. A machine that was
  off gets it from whichever peer it next exchanges fleet state with.

So a preview made on one machine reaches a phone through whichever paired machine is on, over
the connection the phone already has. No address to open, no file server.

**From GitHub.** For `stable`, a program may also ask the project's releases page over HTTPS, as
`meta.update` does today. This is what a fleet with no preview builder relies on, and what a
node on a timer uses.

A check asks every source it can reach and takes the newest set that passes the rule.

## How each kind of install updates

| Install | How it updates |
| --- | --- |
| Android app (APK) | The app downloads the APK through a paired machine, checks it, and hands it to Android's package installer. Android also requires the APK to be signed with the same app key as the installed one. The first time, Android opens **Install unknown apps** once so the person allows Legion to install updates. After that, on Android 12 and later, Legion declares `UPDATE_PACKAGES_WITHOUT_USER_ACTION` and asks for `USER_ACTION_NOT_REQUIRED`, so Android applies updates without a confirmation prompt and restarts Legion onto the new version whenever no competing store holds update ownership and Android's 30-second silent-update throttle is not active; otherwise Android shows its standard "Update this app?" confirmation dialog. |
| macOS app | Replaces its own bundle and restarts. |
| Windows app | Runs the new installer silently and restarts. |
| Linux app, AppImage | Replaces its own file and restarts. |
| Linux app or node from a package (`deb`, pacman) | Never replaces files a package manager owns. It says a newer version exists and gives the one command to run. A package repository that makes this automatic is separate work. |
| Node, standalone | `meta.update`, with its existing preflight, `.prev` copy and restore. With `[update] auto` set in `node.toml` (`off`, `stable` or `preview`; default `off`), a node that receives a newer set it trusts updates itself as a background job a minute later, and a timer unit can check GitHub daily. |
| iPhone and iPad app | Cannot update itself; Apple allows that only through the App Store or TestFlight. |
| Browser | Always as new as the node that serves it. |

Every self-replacing install keeps the previous copy until the new one has started once, and
goes back to it when the new one does not start.

## What a person sees

Settings > About gains **Updates**:

- One line of state: "Up to date", "Version 0.2.0 is ready", "Downloading…", "Restart to finish",
  or a sentence for what went wrong.
- A button when there is something to do: "Update now".
- "Update automatically" (on by default): download when a newer version is seen and install it
  the next time the app is opened or goes idle, never in the middle of something the person
  started.
- "What is new" with the set's notes, and after an update a small one-time line on the first
  screen: "Updated to 0.2.0 · What is new".
- With Technical details: channel, build, sequence, the key that signed it, where it came from.

The app checks when it starts, when it comes back to the front after an hour or more, and every
six hours while open. A check that finds no machine reachable says nothing.

On a machine screen, a node that is behind the newest set the fleet holds says so, with "Update"
for those allowed to (`effect:admin`) and the package manager's command for package installs.

## Making a preview

`scripts/make-update-set.mjs` assembles a set from built artifacts, writes `update.json` and
`SHA256SUMS`, and signs with the key it is given. `legion update.publish from=<dir>` on any
machine of the fleet puts it in circulation. A stable release is the same set attached to a
GitHub release and signed with the offline key by `scripts/sign-legion-release.mjs`.

The Android version code (derived from the UTC `YYYYMMDDHHMM` sequence as Unix minutes, or small
positive test sequences `1..=20_000_000`) and the desktop bundle versions (`<version>+<sequence>`
on Linux, `CFBundleVersion=<sequence>` on macOS) are derived from the sequence, so a platform's own
downgrade protection agrees with Legion's.
