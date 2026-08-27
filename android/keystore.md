# The release signing key

Android will not install an APK that is not signed, and it will not update an installed app unless
the update is signed with the same key. That single rule is the whole reason this file exists.

## Where it lives

| What | Path | Mode |
| --- | --- | --- |
| Keystore | `~/.legion-control/android-release.jks` | 600 |
| Password | `~/.legion-control/android-release.pw` | 600 |

Both live outside the repository, next to the agent state the Mac app already keeps in
`~/.legion-control`. Neither is in git and neither can be: `.gitignore` refuses `*.jks`, `*.keystore`
and `*.pw` anywhere in the tree, so an accidental copy into `android/` still will not be committed.

`android/build-apk.sh` creates both on its first run and then leaves them alone forever:

- The password is 48 hex characters from `openssl rand`, written under `umask 077`. It is never
  printed, never passed on a command line where `ps` could read it, and never echoed by the build.
  Both `keytool` and `apksigner` take it as `file:` so it only ever travels as a path.
- The key is RSA 4096, `SHA256withRSA`, alias `legion-control`, valid for 9862 days, which is about
  27 years. ed25519 is what the app uses to talk ssh, but it is not usable here: none of the APK
  signature schemes accept an Edwards curve key, so APK signing is RSA. The certificate's
  distinguished name is `CN=Legion Control` unless `LEGION_ANDROID_KEY_DNAME` says otherwise.
- The container is PKCS12 despite the `.jks` name. JKS is the old proprietary format and `keytool`
  nags about it on every use. `apksigner`, `jarsigner` and Gradle all read PKCS12 by content, not by
  extension, so the familiar name was kept and the format was not.

## Back it up

If this keystore is lost, the app on the phone can never be updated again. Not "is awkward to
update": Android compares the signing certificate on every install and rejects a differently signed
APK with `INSTALL_FAILED_UPDATE_INCOMPATIBLE`. The only way forward is to uninstall the app and
install the new one, and uninstalling wipes app-private storage, which is where the device's ssh
private key lives. So losing the keystore costs a reinstall plus adding a fresh public key to
`authorized_keys` on every system.

Back up both files together, because the keystore without its password is scrap:

```sh
cp ~/.legion-control/android-release.jks ~/.legion-control/android-release.pw <somewhere safe>
```

Somewhere safe means somewhere that is not the machine you build on and not a repository. A
password manager attachment is fine. A Downloads folder is not.

## Looking at it

The password file is an argument, so this prints nothing secret:

```sh
export JAVA_HOME=/path/to/jdk-21
"$JAVA_HOME/bin/keytool" -list -v \
  -keystore ~/.legion-control/android-release.jks \
  -storepass:file ~/.legion-control/android-release.pw
```

To check what an APK was actually signed with, ask the APK rather than the keystore:

```sh
<sdk>/build-tools/<version>/apksigner verify --print-certs --verbose \
  android/app/build/outputs/apk/release/app-release.apk
```

The SHA-256 digest it prints is the identity Android compares on update. If it matches the installed
copy, the update goes through.

## How the signing actually happens

`build-apk.sh` runs `assembleRelease` and then signs the result itself:

1. `zipalign -p -f 4` on the unsigned APK, because alignment has to happen before signing. A signed
   APK cannot be realigned afterwards without breaking the signature.
2. `apksigner sign --ks ... --ks-pass file:...`, which applies v1, v2 and v3 to the APK.

   There is no `--key-pass`, on purpose. PKCS12 keeps the key under the store password and apksigner
   knows it. Passing `--key-pass file:` at the same file actually breaks the build: apksigner reads
   both passwords from one open stream, so the second read hits end of file and it fails with
   `Failed to read Key "legion-control" password`.
3. `apksigner verify --print-certs --verbose` on the result, printed in full, so the build cannot
   claim success over an APK that would not install.

Read the verify output carefully, because it looks alarming and is not. All three schemes are on the
APK, but verification is done at the APK's own `minSdk`, and at 30 the only one Android consults is
v3, so the report says so:

```
Verifies
Verified using v1 scheme (JAR signing): false
Verified using v2 scheme (APK Signature Scheme v2): false
Verified using v3 scheme (APK Signature Scheme v3): true
```

Those two `false` lines mean "not consulted at this API level", not "missing". Pass
`--min-sdk-version 18` to the same command and all three come back `true`. The line that decides
whether the APK is good is the first one, `Verifies`.

This deliberately needs no change to `app/build.gradle.kts`. AGP writes `app-release-unsigned.apk`
when the release build type has no `signingConfig`, and that is exactly the input this wants. The
build file stays free of any path into a home directory and of anything that has to read a secret.

If the signing config is ever wanted inside Gradle instead, so that `./gradlew assembleRelease`
alone produces a signed APK, this is the exact snippet to add to `android/app/build.gradle.kts`.
`build-apk.sh` already handles that case: it notices AGP produced `app-release.apk` and verifies it
rather than signing a second time.

```kotlin
import java.util.Properties

android {
    signingConfigs {
        create("release") {
            val keystoreFile = File(System.getProperty("user.home"), ".legion-control/android-release.jks")
            val passwordFile = File(System.getProperty("user.home"), ".legion-control/android-release.pw")
            if (keystoreFile.exists() && passwordFile.exists()) {
                val password = passwordFile.readText().trim()
                storeFile = keystoreFile
                storePassword = password
                keyAlias = "legion-control"
                keyPassword = password
            }
        }
    }

    buildTypes {
        getByName("release") {
            // Only wire the config up when the key is actually on this machine, so a checkout on a
            // different Mac still builds an unsigned release APK instead of failing at configuration.
            val releaseSigning = signingConfigs.getByName("release")
            if (releaseSigning.storeFile != null) {
                signingConfig = releaseSigning
            }
        }
    }
}
```

Two things to keep in mind if you take that route. The password ends up in memory in the Gradle
daemon and in the configuration cache, which the `apksigner` path avoids. And Gradle only signs with
v1 and v2 by default for a `minSdk` this high, so check `apksigner verify --print-certs` afterwards
rather than assuming.

## Rotating or revoking

There is nothing to revoke. This is a self-signed certificate that never leaves the machine, is not
in a trust chain, and is not registered with Google Play. The APK is installed by hand over adb.

If it is compromised or you simply want a new one, delete both files and run `build-apk.sh` again. It
will generate a new key and password. Then uninstall the app on the phone before installing the
build, for the reason at the top of this file.

The ssh key is a different key with a different lifecycle: it is generated on the phone, it never
leaves it, and revoking it means deleting its line from `authorized_keys` on every system. Losing
the phone is an `authorized_keys` problem, not a keystore problem.
