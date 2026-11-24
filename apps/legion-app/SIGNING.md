# Android release signing

Android refuses to install an unsigned APK, and it refuses to update an installed app unless the
update is signed with the same certificate (`INSTALL_FAILED_UPDATE_INCOMPATIBLE`).

## Where the key lives

| File | Default path | Mode |
| --- | --- | --- |
| Keystore | `~/.legion-control/legion-app-release.jks` | `600` |
| Password | `~/.legion-control/legion-app-release.pw` | `600` |

Set `LEGION_APP_KEYSTORE` to use another path; the password file is always the sibling `.pw` file
beside it. Both stay outside the repository. `.gitignore` blocks `*.jks`, `*.keystore`, `*.pw` and
`*.idsig` everywhere in the tree so an accidental copy cannot be committed.

`apps/legion-app/build-android.sh` creates both files on its first run and reuses them afterwards:

- The password is 48 hex characters from `openssl rand`, written under `umask 077`. It is never
  printed and never passed on a command line; `keytool` and `apksigner` read it via `file:`.
- The key is RSA 4096 (`SHA256withRSA`), alias `legion-app`, valid for 9862 days (about 27 years),
  with distinguished name `CN=Legion` unless `LEGION_APP_KEY_DNAME` overrides it.
- The container format is PKCS12 despite the `.jks` extension, matching `android/keystore.md`.

## Back it up

If you lose the keystore or its password, you cannot upgrade an existing install on a phone or
tablet without uninstalling it first. Uninstalling wipes the app's private data directory, which
holds the device's Ed25519 key and pairings, so every paired machine has to be paired again.

Back up both files together to somewhere off the build machine (such as a password manager
attachment), never inside a repository:

```sh
cp ~/.legion-control/legion-app-release.jks ~/.legion-control/legion-app-release.pw <safe-location>
```

## Inspecting and verifying

Inspect the keystore without revealing the password:

```sh
"$JAVA_HOME/bin/keytool" -list -v \
  -keystore ~/.legion-control/legion-app-release.jks \
  -storepass:file ~/.legion-control/legion-app-release.pw
```

Verify a built APK and print its signing certificate SHA-256:

```sh
apksigner verify --print-certs --verbose <path-to-apk>
```

The SHA-256 digest is the certificate identity Android checks on update. At `minSdk 30`, `apksigner`
reports `Verified using v3 scheme: true` and `false` for v1/v2 because Android 11+ consults v3; the
`Verifies` line is the pass/fail result.

## Rotating

This is a self-signed key for direct installs. To rotate it, delete both files, run
`apps/legion-app/build-android.sh` to generate a fresh pair, and uninstall the old copy from your
devices before installing the newly signed APK.
