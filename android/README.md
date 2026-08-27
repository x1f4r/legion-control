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

That is the whole thing. It pins the toolchain, creates a signing key if this is the first run,
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

**A configuration, fetched from a machine.** The machines carry the setup (see "Sharing the
setup between devices" in `docs/configuration.md`): the Mac pushes its controller config to
every agent it reaches, and the phone only needs one address to get all of it. On first launch
the Machines page asks for a host, a port and a user; **Fetch** runs `config` there with the
phone's key and applies what comes back. From then on the phone refetches by itself whenever a
machine reports a newer copy, so an edit on the Mac reaches the phone with nothing to do.

The same block lives under **This device → Configuration**, next to the JSON field for pasting
a config by hand (`ssh` and `local` are ignored on the phone; **Insert example** drops in a
template whose hardware address is a placeholder). Either way the document is validated first
and nothing is stored until it passes. The machines' endpoints are dialled directly by the
app's own ssh client, in the order listed: a tunnel address first works from anywhere, a LAN
address as the fallback works from home.

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

**Host keys are pinned per address.** A LAN address shared by the systems of a dual boot
machine legitimately answers with a different host key per system, so it keeps one key per
configured system; a remote address keeps one. The first time an address offers an unknown key
the app asks rather than trusting it quietly. Keys for addresses the configuration no longer
names are dropped when it changes.

**Wake on LAN only works at home.** A magic packet is a broadcast and a broadcast cannot cross
a tunnel. When the phone has no address under the machine's `lanPrefix` the wake action is
disabled with a sentence saying why.

**Updates need nothing.** The app checks the releases of the repository in
`appUpdates.githubRepo` for a newer build of itself and installs it through the system
installer. No account, no token.

## The pages

Reachable from the hamburger in the top bar or by swiping. The drawer and the swipe are the
same state, so neither can be out of step with the other.

| Page | What lives there |
| --- | --- |
| One per machine | Which system is running, the machine name, the agent version, which address answered ("over Home LAN"), and Refresh, Wake, Sleep, Boot into each other system, plus a button per configured action |
| One per service on it | Versions, health, what it is doing now, the relay, the last update, Update now, Restart, and the per system automatic update switches |
| This device | The app's own version and self update, this phone's ssh key, and the configuration: fetch it from a machine, or paste it |

Two things about the actions are worth knowing. **Update now is dead unless a newer version was
actually found**: an agent that could not reach the registry does not know, and an update
pressed on the strength of an unknown stops the service to install nothing. The reason it is
dead is written underneath it. **Sleep, boot, restart and busy-gated actions ask first**: the
agent checks the machine as it is at that moment, refuses while anything is busy, and only then
offers to do it anyway.

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
