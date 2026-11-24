# Legion app (`apps/legion-app`)

`apps/legion-app` is the Tauri 2 native shell for Legion on Android, iOS, iPadOS, macOS, Windows
and Linux ([design](../../docs/legion/platforms.md)). It ships the static frontend from `apps/ui`
directly (no npm or bundler step) and exposes a small Rust command surface over an internal
`Backend` trait.

Its Rust crate (`apps/legion-app/src-tauri`) is its own Cargo workspace with its own `Cargo.lock`,
excluded from the root workspace so building the `legion` node binary never pulls in a web view.
The crate enforces `unsafe_code = "forbid"`.

## Prerequisites

- Rust `1.100.0` (matching the repository's `rust-toolchain.toml`)
- `tauri-cli` `2.12.6`:
  ```sh
  cargo install tauri-cli --version =2.12.6 --locked
  ```
- **Linux desktop builds:** `webkit2gtk-4.1`, `gtk3`, `libayatana-appindicator`, `librsvg`,
  `xdotool`, `patchelf`, `openssl`, `file`, `curl`, and a C toolchain (plus `xvfb` and `dbus-x11`
  for headless smoke checks).
- **Android builds:** JDK 21 (`JAVA_HOME`), Android SDK (`ANDROID_HOME`) with `platform-tools`,
  `platforms;android-37.0`, `build-tools;37.0.0`, `ndk;30.0.16248370` (`NDK_HOME`), and the four
  Rust Android targets (`aarch64-linux-android`, `x86_64-linux-android`,
  `armv7-linux-androideabi`, `i686-linux-android`).
- **iOS and iPadOS builds:** macOS with Xcode (and an iOS Simulator runtime installed) and the Rust
  targets `aarch64-apple-ios` and `aarch64-apple-ios-sim`.
- **macOS desktop builds:** macOS with Xcode or Xcode Command Line Tools.
- **Windows desktop builds:** Windows with MSVC Build Tools ("Desktop development with C++") and
  the WebView2 runtime.

## Frontend staging and shell seams

### Staging `apps/ui`

`src-tauri/build.rs` copies everything in `apps/ui/` except `design/`, `test/`, and dotfiles into
`apps/legion-app/dist/` before Tauri embeds it. `apps/legion-app/dist/.gitkeep` is tracked in git
(and preserved by `build.rs`) so `tauri-cli` finds `frontendDist` (`../dist`) on a clean checkout
before `build.rs` runs. If `apps/ui/index.html` is not present yet, `build.rs` writes a minimal
placeholder page and emits a Cargo warning.

### Startup ready marker (`ready`)

Every platform registers a `ready` command. Once the interface has mounted and painted its first
frame, it calls `invoke("ready")`. The Rust side logs one line — `legion-app: interface ready` —
to stdout (and Android logcat under tag `legion-app`). When `LEGION_APP_READY_FILE` is set in the
environment, it also writes that line to the given file path. Automated checks use this marker
instead of fixed sleeps; in normal runs the environment variable is unset and the command is a
harmless one-time log.

### QR invite scanning (`scan_invite`)

`scan_invite` returns a tagged JSON object (`kind`):

- **Android:** `{ "kind": "scanned", "content": "<invite>" }`. The Rust command checks and
  requests camera permission, runs `tauri-plugin-barcode-scanner` natively, and returns the trimmed
  QR payload (or a structured `AppError` with `denied`, `cancelled`, `invalid_args`, or `failed`).
- **iOS and iPadOS:** `{ "kind": "use_plugin", "plugin": "barcode-scanner", "formats": ["QR_CODE"], "camera_direction": "back", "windowed": false }`.
  `tauri-plugin-barcode-scanner` `2.5.3` keeps its iOS `PluginHandle` private and registering a
  second iOS plugin handle requires `unsafe` FFI (`tauri::ios_plugin_binding!`), which this crate
  forbids. The plugin is registered and permitted in `src-tauri/capabilities/mobile.json`
  (`barcode-scanner:allow-check-permissions`, `allow-request-permissions`, `allow-scan`,
  `allow-cancel`), so when `scan_invite` returns `kind: "use_plugin"`, the interface invokes
  `plugin:barcode-scanner|check_permissions`, `plugin:barcode-scanner|request_permissions`, and
  `plugin:barcode-scanner|scan` (or `window.__TAURI__.barcodeScanner`) directly with those options.
- **Desktop:** `scan_invite` is not registered; desktop users paste invite text or use the local
  `legion rpc` pipe.

### Platform behaviour notes

- **Android safe areas and back navigation:** On Android 17, `MainActivity.kt` enables edge-to-edge
  layout and observes `WindowInsetsCompat` (`systemBars() | displayCutout()`) on `window.decorView`
  without replacing the WebView's own insets listener. Chromium populates CSS `env(safe-area-inset-*)`
  natively, and `MainActivity.kt` also publishes `--safe-area-inset-{top,right,bottom,left}`.
  System back walks the web view history before exiting.
- **iOS and iPadOS safe areas, theme and back gesture:** The web view loads with `viewport-fit=cover`
  so CSS `env(safe-area-inset-*)` clears the Dynamic Island/notch and home indicator on iPhone and
  status/home bars on iPad, and follows system light/dark appearance automatically. Both iPhone and
  iPad are supported (`TARGETED_DEVICE_FAMILY = "1,2"`, deployment target iOS 15.0, all four
  orientations on iPad). Note that while `wry` `0.57.3` supports
  `WKWebView.allowsBackForwardNavigationGestures` (defaulting to `false`), `tauri` `2.12.7` does
  not expose that flag on its window/webview builders or configuration, and `with_webview` on iOS
  exposes only a raw pointer requiring `unsafe`; the edge-swipe back gesture therefore stays off
  until Tauri exposes a safe option.
- **Backup exclusion:** Android excludes all app data in `data_extraction_rules.xml` (`allowBackup=false`).
  On iOS and macOS, `AppStore` marks both the app data directory (on startup) and the state file
  (on every atomic save) with the `com.apple.metadata:com_apple_backup_excludeItem` extended
  attribute via `rustix::fs::setxattr` (safe Rust, no `unsafe`), which sets Apple Foundation's
  `NSURLIsExcludedFromBackupKey` to `true` so pairing keys stay out of iCloud, iTunes and Time
  Machine backups.
- **Desktop window and single instance:** Desktop builds default to `1080x740` (above the `820px`
  wide-layout breakpoint) with a `360x540` minimum and register `tauri-plugin-single-instance` so
  a second launch focuses the existing main window.
- **Content Security Policy:** `tauri.conf.json` enforces `default-src 'none'` and restricts
  scripts, styles and images to `'self'` plus `tauri:` (needed for Apple `WKWebView` custom-scheme
  assets) and `connect-src` to Tauri's IPC endpoints.

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

### macOS desktop (`.app` and `.dmg`)

```sh
cd apps/legion-app
cargo tauri build --bundles app,dmg
```

Produces an ad-hoc signed Apple Silicon `.app` and `.dmg` under
`apps/legion-app/src-tauri/target/release/bundle/`.

### Windows desktop (NSIS `.exe`)

```powershell
cd apps\legion-app
cargo tauri build --bundles nsis
```

Produces the NSIS installer under `apps\legion-app\src-tauri\target\release\bundle\nsis\`.

### iOS and iPadOS (simulator `.app` and unsigned device `.ipa`)

```sh
cd apps/legion-app
cargo tauri ios build --target aarch64-sim --no-sign --ci
cargo tauri ios build --target aarch64 --no-sign --ci
```

The simulator build writes `src-tauri/gen/apple/build/arm64-sim/Legion.app` for `xcrun simctl`.
The device build writes an unsigned `.ipa` under `src-tauri/gen/apple/build/arm64/`.

### Installing onto a real iPhone or iPad with your own Apple ID

CI cannot sign iOS builds because Apple requires a signing identity and provisioning profile tied
to an Apple account. To install Legion on your own iPhone or iPad from a Mac with Xcode:

1. Make sure Rust (`1.100.0` with target `aarch64-apple-ios`) and `tauri-cli` `2.12.6` are
   installed, and clone the repository.
2. Open `apps/legion-app/src-tauri/gen/apple/legion-app.xcodeproj` in Xcode.
3. Select the **legion-app** project in the navigator, choose the **legion-app_iOS** target, and
   open the **Signing & Capabilities** tab.
4. Tick **Automatically manage signing** and pick your Apple ID (**Personal Team** or paid
   Developer team) in the **Team** menu. If Xcode reports that `com.x1f4r.legion` is already taken
   for your account, change **Bundle Identifier** locally (for example `com.example.me.legion`)
   without committing that change.
5. Connect your iPhone or iPad over USB, unlock it, and choose it in Xcode's destination bar. On
   iOS 16 or later, enable **Settings -> Privacy & Security -> Developer Mode** if prompted.
6. Press **Run** (Cmd+R). Xcode builds the Rust library, signs the bundle with your Apple ID, and
   installs it on the device.
7. On the first install with a free Apple ID, open **Settings -> General -> VPN & Device Management**
   on the phone or tablet and trust your developer profile. (Free Apple ID signatures expire after
   7 days and can be refreshed by pressing Run again; for devices without Xcode, use the browser
   door served by `legion serve`.)

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

## What CI verifies (`.github/workflows/legion-app.yml`)

- **Rust check (Linux):** `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`,
  and `cargo test --locked`.
- **Android:** builds the universal debug APK (`arm64-v8a` + `x86_64`), boots an x86_64 Android
  emulator under KVM, installs and launches the APK, waits for `legion-app: interface ready` in
  `adb logcat`, captures dark and light screenshots, and uploads the APK and screenshots.
- **iOS and iPadOS:** builds the `aarch64-sim` `.app`, boots a current iPhone simulator and a
  current iPad simulator, installs and launches Legion on each, waits for `legion-app: interface ready`,
  verifies Foundation reports `NSURLIsExcludedFromBackupKey == true` on the app data directory,
  captures dark and light screenshots on both devices, builds the unsigned device `.ipa`
  (`Legion-ios-unsigned.ipa`), and uploads the screenshots and unsigned `.ipa`.
- **macOS desktop:** runs `cargo test --locked` (including the Apple backup-exclusion attribute
  test), builds the ad-hoc signed Apple Silicon `.app` and `.dmg`, launches the app, waits for
  `legion-app: interface ready`, confirms the app window exists, captures a screenshot when
  permitted, and uploads the `.dmg`.
- **Windows desktop:** builds the NSIS installer (`.exe`), runs a silent install into a temporary
  directory, launches the installed executable under WebView2, waits for `legion-app: interface ready`,
  captures a screenshot when permitted, and uploads the installer.
- **Linux desktop:** builds `.deb` and `.AppImage` bundles, launches the built binary under
  `dbus-run-session` and `xvfb-run`, waits for `legion-app: interface ready`, and uploads the
  bundles.

## Headless Android emulator harness

`apps/legion-app/emulator.sh` drives a headless, loopback-only Android emulator for local testing
and screenshots. The default AVD is `legion-test`; pass `--avd legion-tablet` for the tablet AVD.

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
