# Legion app (`apps/legion-app`)

`apps/legion-app` is the Tauri 2 native shell for Legion on Android, iOS, macOS, Windows and Linux
([design](../../docs/legion/platforms.md)). It ships the static frontend from `apps/ui` directly
(no npm or bundler step) and connects to Legion nodes through `crates/legion-link` behind an
internal `Backend` trait.

## What the app does

- **Pairing and SSH transport:** On every platform, the app enrols with a node from a single-use
  `legion pair` invite (via camera QR scan on Android or pasted invite text), generates an Ed25519
  device key on first use, and keeps its configuration (`LinkConfig`, active node, and node
  metadata) in `legion-app.json` inside the platform app data directory.
- **Desktop local pipe:** On Linux, macOS and Windows, if a `legion` binary is present on `PATH`
  or in a standard user/system install location, the app is ready immediately over a local
  `legion gate` child process without pairing, while still allowing additional SSH nodes to be
  paired.
- **Streaming, failover, and reconnect:** Tool calls stream live output over Tauri IPC channels
  with caller-scoped cancellation, fail over to another configured node on unreachable/timeout
  errors, and reconnect transparently after backgrounding or a dropped node-side session.
- **Revocation and unpairing:** If a node revokes the device (`legion device.remove`), the app
  shows a calm error with a direct path back to pairing. Unpairing forgets the node locally and
  prints the `legion device.remove` command to revoke the key on the node as well.

Its Rust crate (`apps/legion-app/src-tauri`) is its own Cargo workspace with its own `Cargo.lock`,
excluded from the root workspace so building the `legion` node binary never pulls in a web view.

## Prerequisites

- Rust `1.100.0` (matching the repository's `rust-toolchain.toml`)
- `tauri-cli` `2.12.6`:
  ```sh
  cargo install tauri-cli --version =2.12.6 --locked
  ```
- **Linux desktop builds:** `webkit2gtk-4.1`, `gtk3`, `libayatana-appindicator`, `librsvg`,
  `xdotool`, `patchelf`, `openssl`, `file`, `curl`, and a C toolchain.
- **Android builds:** JDK 21 (`JAVA_HOME`), Android SDK (`ANDROID_HOME`) with `platform-tools`,
  `platforms;android-37.0`, `build-tools;37.0.0`, `ndk;30.0.16248370` (`NDK_HOME`), and the four
  Rust Android targets (`aarch64-linux-android`, `x86_64-linux-android`,
  `armv7-linux-androideabi`, `i686-linux-android`).

## Frontend staging

`src-tauri/build.rs` copies everything in `apps/ui/` except `design/` and `test/` into the
gitignored `apps/legion-app/dist/` directory before Tauri embeds it. If `apps/ui/index.html` is not
present yet, the build script writes a minimal placeholder page and emits a Cargo warning.

On Android 17, `MainActivity.kt` enables edge-to-edge layout and observes `WindowInsetsCompat`
(`systemBars() | displayCutout()`) on `window.decorView` without replacing the WebView's own
insets listener. That lets Chromium populate CSS `env(safe-area-inset-*)` natively (verified as
`55px` top / `24px` bottom on phone and `24px` top / `32px` bottom on tablet), and also publishes
`--safe-area-inset-top`, `--safe-area-inset-right`, `--safe-area-inset-bottom` and
`--safe-area-inset-left` CSS variables at document start and on insets/configuration changes.

## Building

### Rust checks (host)

```sh
cargo fmt --manifest-path apps/legion-app/src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path apps/legion-app/src-tauri/Cargo.toml --all-targets --locked -- -D warnings
cargo test --manifest-path apps/legion-app/src-tauri/Cargo.toml --locked
```

### Linux desktop (`.deb` and `.AppImage`)

```sh
cd apps/legion-app
cargo tauri build --bundles deb,appimage
```

Bundles land in `apps/legion-app/src-tauri/target/release/bundle/`. On systems without FUSE (such
as CI runners), set `APPIMAGE_EXTRACT_AND_RUN=1`.

### Android debug APK

```sh
cd apps/legion-app
cargo tauri android build --debug --apk --target aarch64 --target x86_64
```

### Android signed release APK

```sh
./apps/legion-app/build-android.sh             # arm64-v8a release APK
./apps/legion-app/build-android.sh --emulator  # arm64-v8a + x86_64 release APK
```

The script aligns the APK with `zipalign`, signs it with the release keystore outside the
repository (`~/.legion-control/legion-app-release.jks`, created on first run), verifies the
signature with `apksigner`, and prints the path, size and certificate SHA-256. See
[SIGNING.md](SIGNING.md) before losing or rotating the key.

## Headless Android emulator harness

`apps/legion-app/emulator.sh` drives a headless, loopback-only Android emulator for testing and
screenshots. The default AVD is `legion-test`; pass `--avd legion-tablet` for the tablet AVD.

```sh
./apps/legion-app/emulator.sh start
./apps/legion-app/emulator.sh install path/to/app.apk
./apps/legion-app/emulator.sh launch
./apps/legion-app/emulator.sh theme dark
./apps/legion-app/emulator.sh shot /home/me/shots/phone-dark.png
./apps/legion-app/emulator.sh theme light
./apps/legion-app/emulator.sh shot /home/me/shots/phone-light.png
./apps/legion-app/emulator.sh tap 540 1200
./apps/legion-app/emulator.sh back
./apps/legion-app/emulator.sh text 'hello'
./apps/legion-app/emulator.sh logcat
./apps/legion-app/emulator.sh stop
```

## End-to-end Android test (`e2e-android.sh`)

`apps/legion-app/e2e-android.sh` proves the full Android path against a real throwaway Legion node
and loopback `sshd` (`127.0.0.1` on an ephemeral high port):

```sh
./apps/legion-app/e2e-android.sh
./apps/legion-app/e2e-android.sh --node-ref origin/link/pairing --shots-dir /home/me/shots/e2e
./apps/legion-app/e2e-android.sh --skip-tablet
```

The script builds a release `legion` binary from `--node-ref` (default `origin/link/pairing`) in a
cache checkout under `${XDG_CACHE_HOME:-$HOME/.cache}/legion-app/`, creates a throwaway
`LEGION_HOME` node named `tower` with principal `phone` in role `operator`, starts a loopback
`sshd` using a throwaway host key and `authorized_keys` file, builds a WebView-debuggable `x86_64`
debug APK (used only by this test), installs it on `legion-test`, and drives the real UI over
`adb forward` + Chrome DevTools Protocol (`playwright-core` from `apps/ui/test`). It verifies
first-start pairing, live machine metrics, streamed command execution, file listing, `meta.whoami`,
power confirmation with dry run only, force-stop/relaunch persistence, transparent reconnect after
killing the node-side session, revocation recovery and re-pairing, plus a wide paired screenshot on
`legion-tablet`. It never touches `~/.ssh`, the system `sshd`, `~/.config/legion`, or `/tmp`.
