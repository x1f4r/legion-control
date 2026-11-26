# Linux packaging for `legion`

This directory holds the Debian/Ubuntu (`.deb`) and Arch Linux (`PKGBUILD`) packaging for the
`legion` node binary, plus its manual page and optional systemd user unit.

The package name is `legion` on Debian, Ubuntu, and Arch (and `legion-bin` for the prebuilt static
binary variant on Arch). No package named `legion` and no `/usr/bin/legion` binary exist in Debian,
Ubuntu, Arch official repositories, or the AUR.

## What every package installs

- `/usr/bin/legion` — the `legion` executable
- `/usr/lib/legion/managed-by` — ownership marker (`deb` or `pacman`) so `meta.update` and
  `fleet.install` refuse in-place replacement and direct you to `apt` or `pacman`
- `/usr/lib/systemd/user/legion-serve.service` — optional systemd user unit for `legion serve`
  (loopback only; never enabled or started automatically)
- `/usr/share/man/man1/legion.1.gz` — manual page
- Licence and documentation files (`/usr/share/doc/legion/` on Debian/Ubuntu,
  `/usr/share/licenses/<pkgname>/LICENSE` on Arch)

Packages never start services or touch `~/.config/legion/` on install, upgrade, or removal. Run
`legion init` after installing, and enable the user unit only if you want `legion serve` running
in the background:

```sh
systemctl --user enable --now legion-serve.service
```

## Debian and Ubuntu (`.deb`)

### Build

First compile a static musl binary for the target architecture, then pass it to
`packaging/deb/build-deb.sh`:

```sh
cargo build --release --locked --target x86_64-unknown-linux-musl -p legion-cli
bash packaging/deb/build-deb.sh \
  target/x86_64-unknown-linux-musl/release/legion \
  amd64 \
  0.1.0 \
  target/packages
```

For `arm64`, build for `aarch64-unknown-linux-musl` and pass `arm64`. `build-deb.sh` compresses the
archive with `-Zgzip` and needs only `dpkg-deb`, `gzip`, and `md5sum`.

### Install, upgrade, and remove

```sh
# Install or upgrade from a downloaded or locally built .deb
sudo apt-get install ./legion_0.1.0-1_amd64.deb

# Remove the package (disable legion-serve.service first if you enabled it;
# leaves ~/.config/legion and ~/.local/state/legion intact)
sudo apt-get remove legion
```

## Arch Linux (`PKGBUILD`)

Three `PKGBUILD` files live under `packaging/arch/`:

- `packaging/arch/PKGBUILD` (`legion`) — builds the node from source at a `legion-v<version>`
  release tag, records the tag's commit in `LEGION_BUILD_REV` (from the tag archive header or remote
  tag), and remaps build paths out of the binary with `--remap-path-prefix`. Requires Rust matching
  `rust-toolchain.toml` (`1.100.0` or newer, for example through `rustup` or Arch's `rust` package
  once updated).
- `packaging/arch/legion-bin/PKGBUILD` (`legion-bin`) — installs the released static musl binary,
  verifying `SHA256SUMS.sig` against the pinned Ed25519 release public key from
  `contract/release-public-key.pem` (via `openssl`) and checking the binary's SHA-256 digest in
  `prepare()`, and provides/conflicts with `legion`.
- `packaging/arch/legion-app/PKGBUILD` (`legion-app`) — builds the desktop app from source with
  `--features tauri/custom-protocol`, installing `/usr/bin/legion-app`, its `.desktop` file, and
  hicolor icons.

### Build from a published tag

```sh
cd packaging/arch
makepkg -si
```

For the prebuilt binary node variant or desktop app, run the same command inside
`packaging/arch/legion-bin/` or `packaging/arch/legion-app/`.

### Build from a local checkout (before a release tag exists)

Run `packaging/arch/build-local.sh` as a non-root user on Arch:

```sh
# Build the source node package from this working tree
bash packaging/arch/build-local.sh source target/packages

# Or build the -bin node package from a locally built static musl binary
bash packaging/arch/build-local.sh bin \
  target/x86_64-unknown-linux-musl/release/legion \
  target/packages

# Or build the desktop app package from this working tree
bash packaging/arch/build-local.sh app target/packages
```

### Install, upgrade, and remove

```sh
# Install or upgrade a built package file
sudo pacman -U target/packages/legion-0.1.0-1-x86_64.pkg.tar.zst

# Remove the package (disable legion-serve.service first if you enabled it;
# leaves ~/.config/legion and ~/.local/state/legion intact)
sudo pacman -R legion
```
