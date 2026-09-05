# Legion Control for Android

The phone version of the Mac app. It is a port, not a second system: it runs the same
`legionctl` agent over ssh, sends the same commands, and parses the same one-line JSON contract.
Nothing on a controlled machine knows or cares which client is talking to it.

`minSdk` is 30, so anything from Android 11 onwards will install it. The build ships arm64
only, since the other ABIs only add weight; `-Plegion.abiFilters=arm64-v8a,x86_64` overrides
that.

## Build it

```sh
./android/build-apk.sh
```

Build the signed agent bundle with `scripts/package-agent.mjs` first; the release script requires
its archive, manifest and signature in `dist/`. It pins the toolchain, creates a signing key if this is the first run,
builds a release APK, signs it, verifies the signature and prints where it landed:

```
android/app/build/outputs/apk/debug/app-debug.apk      # ./gradlew assembleDebug
android/app/build/outputs/apk/release/app-release.apk  # ./android/build-apk.sh
```

The release APK is about 28 MB across four dex files, which is more than a handful of screens
deserves and is not a symptom of anything being wrong. R8 is deliberately off, so Bouncy Castle
and Compose both go in whole. `app/build.gradle.kts` explains that decision: sshj picks its
ciphers and key exchanges out of name-keyed factory lists, which shrinking quietly empties.

Other flags:

| Flag | Does |
| --- | --- |
| `--install` | pushes the APK to the connected phone with adb afterwards |
| `--clean` | throws the previous build output away first |
| `--help` | prints the header of the script |

To build by hand instead, set `JAVA_HOME` to a JDK 21 every single time:

```sh
cd android
JAVA_HOME=/path/to/jdk-21 ./gradlew --no-daemon assembleDebug
```

## The toolchain, and why it is pinned

This combination was arrived at the hard way. Do not "modernise" it without reading this.

**Gradle 9.5, through the wrapper in this directory.** Never `gradle` off PATH. AGP 8.13 calls
`org.gradle.api.problems.internal.InternalProblems`, which Gradle 9.6 removed. The build does
not fail somewhere useful, it fails while applying the Android plugin, with a message telling
you to use Gradle 9.5. Always `./gradlew`.

**JDK 21.** AGP rejects newer JDKs outright. `build-apk.sh` exports `JAVA_HOME` itself and
refuses to continue if the JDK there is not 21; point `LEGION_ANDROID_JAVA_HOME` at yours if
it is not at the Temurin default location.

**AGP 8.13.0, compileSdk 36, targetSdk 36, minSdk 30, Kotlin 2.x with the Compose compiler
plugin.**

**`android/local.properties` needs `sdk.dir=<your Android SDK>`.** It is gitignored, because it
is a path on one machine. `build-apk.sh` writes it on the first run if it is missing, from
`ANDROID_SDK_ROOT`, `ANDROID_HOME`, or the Android Studio default.

One trap worth writing down: regenerating the wrapper with `gradle wrapper` inside this
directory does not work. Configuring the project applies AGP, and AGP on a newer Gradle is the
thing that explodes, so the wrapper task never gets to run. Generate it in a scratch directory
containing nothing but a `settings.gradle.kts`, then copy `gradle/`, `gradlew` and
`gradlew.bat` back into `android/`.

### The wall of D8 stack traces is expected

Every full build prints a few hundred lines of this, once per class, and the build still
succeeds:

```
WARNING: D8: An error occurred when parsing kotlin metadata.
WARNING: D8: Unexpected error during rewriting of Kotlin metadata for class '...':
com.android.tools.r8.internal.xb4: Should never be called
```

Kotlin 2.3 writes a `kotlin.Metadata` version that the R8 bundled with AGP 8.13 does not know
how to read, so D8 gives up rewriting it and copies it through unchanged. Nothing in this app
reads that annotation: kotlinx.serialization goes through generated `$serializer` classes, and
there is no Kotlin reflection anywhere. The APK is correct.

It is noise rather than a problem, but it is enough noise to bury a real error, so when a build
fails do not read the traces, read the last twenty lines.

## Install it

Over the LAN, with adb already paired to the phone:

```sh
./android/build-apk.sh --install
```

If adb has lost the phone, wireless debugging keeps its port alive only while it is advertised:

```sh
adb mdns services          # look for _adb-tls-connect._tcp
adb connect <host:port>
```

The first install after switching between a debug build and a release build fails with
`INSTALL_FAILED_UPDATE_INCOMPATIBLE`, because the two are signed with different keys.
Uninstall first. That wipes the app's ssh key, so see the next section.

## What the app needs before it works

**A setup, fetched from a machine.** Every machine carries the setup and every device is a
peer: the phone edits and publishes it exactly as the Mac and the desktop do, and there is no
device whose copy wins by rank. On first launch the Machines page asks for a host, a port and a
user; **Fetch** runs `config` there with the phone's key and applies what comes back.

After that it keeps itself in step on every poll. Two things happen without asking, and both
only along strict descent: the phone pushes to a machine whose copy its own was made from, and
it adopts a machine's copy that was made from its own. Anything else stops and asks. Two people
editing while one is offline produce two revision 6s, and a revision number cannot tell that
apart from one being newer; the ancestry can, so the document carries the hashes of the
revisions it came from and the app merges rather than overwriting. See "Editing the setup"
below.

The same block lives under **This device → Setup**, next to the JSON field for pasting a
document by hand (**Insert example** drops in a template whose hardware address is a
placeholder). Either way the document is validated first and nothing is stored until it passes.
The machines' endpoints are dialled directly by the app's own ssh client, in the order listed:
a tunnel address first works from anywhere, a LAN address as the fallback works from home.

**The public key has to be authorised on every system.** The app generates its own ed25519
keypair on the phone on first run and keeps the private half in app-private storage, wrapped by
a key in the Android keystore. No key is shipped in the APK, and no other device's key is
reused, because a phone is the easiest thing to lose and a separate key means revoking it is
deleting one line rather than re-keying everything.

The app shows its public key with a copy button. It has to go into `authorized_keys` on every
system of every machine: `~/.ssh/authorized_keys` on Linux and macOS; on Windows,
`C:\ProgramData\ssh\administrators_authorized_keys` for an administrator account, otherwise
`C:\Users\<user>\.ssh\authorized_keys`. A dual boot machine has two systems and only one is up
at a time, so doing only one half means the app works on some days and not others. Until the
key is authorised, ssh fails with a permission error and the app shows the public key screen
instead of a generic failure.

**Host keys are pinned per host and port, grouped by locally approved operating systems.**
First contact shows the offered fingerprint and asks you to confirm the OS identities using that
address, then select which OS owns the key. The shared setup only suggests names; editing it
cannot authorize new identities. A dual-boot address keeps both approved systems, including
multiple key algorithms assigned to one OS. An unknown key may fill only an empty, locally
reserved OS identity; it cannot replace an existing one. Existing legacy pins keep working and
must be explicitly assigned before another OS is enrolled. The app prefers already pinned SSH
algorithms. Shared setup changes never remove pins or enlarge local trust reservations.

**Waking needs somebody on the machine's own network.** A magic packet is a link-local
broadcast and a tunnel has no broadcast domain to put one on, so the phone can only wake a
machine it is standing next to. For everything else the setup names **helpers**: machines that
are already on that network and have an action that sends the packet, tried in the order they
are listed. A helper is never woken automatically, because that would turn an always-off tower
into an always-on one by accident; where one could be woken, the app offers that as a separate
press.

Sites make this work across houses. A site is one network with its address prefixes and its
broadcast address, and a machine says which site it is at. Two houses behind the same router
model have the same private subnet, so where several sites match the phone's address the app
says the site is unconfirmed rather than picking one, and **This device** has a setting to say
which it is. That setting only decides where a packet is sent from; what proves a machine is
the machine it claims to be is always its pinned host key.

**Updates check a signature before they check anything else.** The app reads the release named
by `appUpdates.githubRepo`, takes `Legion-Control-manifest.json` and its detached signature by
exact name, and verifies them against the release key pinned in `data/Trust.kt`. Only then does
it read anything out of the manifest, take the artefact named exactly
`Legion-Control-android-arm64.apk`, check its size and sha256 against the manifest, and look
inside the archive to confirm the package name, the version and the signing certificate match
the app that is running. The system installer's own check is the last of those, not the first.
No account, no token, and no way to configure a different key.

## The pages

Reachable from the hamburger in the top bar or by swiping. The drawer and the swipe are the
same state, so neither can be out of step with the other.

| Page | What lives there |
| --- | --- |
| One per machine | Which system is running, the machine name, the agent version and contract, which address answered, which setup copy it holds, the changes in flight and the last few outcomes, Refresh, Wake, Sleep, Boot into each other system, a button per configured action, and Diagnostics |
| One per service on it | Versions, health, what it is doing now, the relay, the last update, when the schedule may touch it, Update now, Update when idle, Restart, Restart when idle |
| Edit the setup | Machines, addresses, systems, sites and wake helpers, with the raw document one page along |
| Setup edited twice | Two copies neither made from the other, with a per-entry merge |
| This device | The app's version and self update, this phone's key, what this device is called and where it is standing, whether finished changes are announced, the setup, and everything this phone has asked for |

**Service setup** edits the full agent configuration with templates, a validation preview, and an
explicit Save. It requires an unrestricted administrator SSH key. The hash read with the document
prevents an intervening change from being overwritten; saving does not execute setup commands.
Configured telemetry appears only when the agent returns named readings. Unavailable values remain
unavailable, with probe errors under Details; the phone does not retain a telemetry history.

**Change details** keeps the result message, action output and operation log separate. Saved local
history remains readable when the machine is offline. Diagnostics offers address checks, agent
checks and an explicit deep check; logs and secondary results are shown on demand.

Four things about the actions are worth knowing.

**Update now is dead unless a newer version was actually found**: an agent that could not reach
the registry does not know, and an update pressed on the strength of an unknown stops the
service to install nothing. The reason it is dead is written underneath it.

**Asking for an update is not the same as the schedule running one.** Turning automatic updates
off stops the scheduler and nothing else; Update now still works, and is still refused while
the machine is working. Pausing and maintenance windows are the scheduler's business too.

**Sleep, boot, restart and busy-gated actions ask first**, and each question has a second,
quieter answer: do it when the machine is next idle. The agent checks the machine as it is at
that moment, and only after it has looked and said no is "do it anyway" offered. Force skips the
busy check and nothing else: never the machine's lock, never a broken configuration, and never
an install the agent could not verify.

**A change that was sent and whose reply was lost is never sent again on its own.** It gets an
id before anything leaves the phone, the record survives the app being closed, and the app asks
the machine what became of that id rather than repeating the command. Being told to reboot is
not the same as having rebooted: the row says the outcome is not known until the machine answers
as the system that was asked for.

## Editing the setup

Every device is a peer. The **Edit the setup** page is a form over the document itself rather
than over a typed model, which matters for one reason: a key written by a newer client, or by
somebody's hand, has to survive an edit made here. A typed round trip would drop it silently and
the document would then differ on every device that had not been edited from a phone.

An edit becomes this device's setup the moment it is applied, whether or not any machine is in
reach, and travels on the next poll of each one. Behind that:

- The document carries `controller.id` (the setup, shared by every peer), `controller.revision`
  and `controller.lineage`, the hashes of up to 32 ancestors.
- A machine accepts a push only from a copy that descends from what it holds. Anything else it
  refuses, so a concurrent edit is surfaced rather than lost.
- The phone pushes only where its copy descends from the machine's, and adopts only where the
  machine's descends from its own. Descent has a direction, so two devices can never take turns
  overwriting each other.
- Anything else is a question. **Setup edited twice** shows both revisions, who wrote them, and
  a difference per machine, address, system, wake block or site, defaulting to whichever side
  actually changed each one. Merging writes a revision descending from both, which every machine
  then accepts as an ordinary fast-forward, so nothing is replaced.
- A machine carrying an entirely different setup cannot be merged with, and is the one place the
  app ever sends `--replace`, after an explicit decision.

The hash all of this turns on is the canonical form from `contract/hash-vectors.json`: strict
UTF-8, one leading byte order mark stripped, CRLF and CR to LF, a fixed six-character whitespace
set trimmed from both ends, exactly one trailing newline. `data/CanonicalSetup.kt` implements
that rule and `CanonicalSetupTest` checks every shared vector, including the ones that must be
refused.

## Testing on an emulator

Debug builds only, and absent from a release APK entirely. The receiver is exported and locked
to `android.permission.DUMP`, which the adb shell holds and an ordinary app cannot get.

```sh
# The line to add to authorized_keys, printed to logcat.
adb shell am broadcast -a com.x1f4r.legioncontrol.debug.SHOW_KEY   -n com.x1f4r.legioncontrol.debug/com.x1f4r.legioncontrol.debug.TestBridgeReceiver
adb logcat -d -s LegionControlTest

# Apply a setup document.
adb shell am broadcast -a com.x1f4r.legioncontrol.debug.APPLY_CONFIG   -n com.x1f4r.legioncontrol.debug/com.x1f4r.legioncontrol.debug.TestBridgeReceiver   --es config "$(cat controller.json)"

# Which setup this device now holds.
adb shell am broadcast -a com.x1f4r.legioncontrol.debug.SHOW_SETUP   -n com.x1f4r.legioncontrol.debug/com.x1f4r.legioncontrol.debug.TestBridgeReceiver
```

No test key, host or address is compiled into any build; everything arrives as an extra. From an
emulator the host machine is `10.0.2.2`, so a fixture endpoint has to be given as an address the
emulator can actually reach.

## The signed agent bundle

A release build carries the signed control agent so that putting contract 3 on a machine running
an older one is one action rather than a terminal session. The Gradle build copies three files
out of the repository's `dist/` directory into `assets/agent/`:

```
legionctl-agent-<version>.tgz
Legion-Control-agent-manifest.json
Legion-Control-agent-manifest.json.sig
```

They are gitignored: they are build outputs of `scripts/package-agent.mjs`, signed by the
release key, and a copy in source control would be one more place for the two to drift apart. A
build without them is normal, every test passes without them, and the install action then says
this build carries nothing to install rather than offering something unverified. The app checks
the manifest signature against the pinned key and the tarball against the manifest before a byte
of it is sent anywhere; the machine checks it again before it swaps anything.

## Signing

`~/.legion-control/android-release.jks`, password in `~/.legion-control/android-release.pw`,
both created on the first build, both outside the repository, both gitignored. **Back the
keystore up.** Losing it means the installed app can only ever be updated by uninstalling it
first, which throws away the phone's ssh key. Full detail in [keystore.md](keystore.md).

## Layout

```
android/
  build-apk.sh                one command to a signed release APK
  keystore.md                 the signing key: where it is, how it works, why to back it up
  README.md                   this file
  gradlew                     Gradle 9.5, pinned, use this and never the one on PATH
  settings.gradle.kts
  gradle/libs.versions.toml   every pinned version, with the reason next to it
  app/
    build.gradle.kts
    proguard-rules.pro        R8 keep rules for sshj, kept ready but not used while R8 is off
    src/main/java/...         the app
```
