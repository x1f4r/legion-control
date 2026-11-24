# Contributing

Legion is a Rust workspace (`crates/`), a static web interface (`apps/ui/`), Linux packaging
(`packaging/`), and supporting scripts and tests (`scripts/`, `tests/`). This is what a change is
held to before it is merged.

## Checks

    ./scripts/check.sh scripts   # JS syntax, invisible characters, script tests, UI unit tests, private data
    ./scripts/check.sh rust      # cargo fmt, cargo clippy, cargo test, generated tool reference check

`./scripts/check.sh` with no argument runs both legs. CI runs the same legs on Linux, macOS, and
Windows, together with:

- `legion-link` cross-compilation checks for Android and iOS targets;
- ShellCheck (`shellcheck -S warning` on every tracked `*.sh` file) and PSScriptAnalyzer (`./scripts/check-powershell.ps1` on every tracked `*.ps1` file);
- headless browser end-to-end UI tests (`node --test apps/ui/test/browser.test.mjs`);
- real-SSH end-to-end tests (`.github/workflows/legion-e2e.yml`) on Linux, macOS, and Windows;
- Linux `.deb` and Arch `PKGBUILD` container verification (`.github/workflows/legion-packages.yml`).

When you add or change a built-in tool or its schema, regenerate the tool reference before
committing:

    cargo build -p legion-cli
    node scripts/legion-tools-doc.mjs

## The repository is public

Write placeholders, never values from a real machine or network:

- user `me`, and homes `/home/me`, `/Users/me` and `C:\Users\me`;
- machines `tower`, `pi`, `nas`, `laptop`, `old-box`, `studio`, `shed` and `tv`;
- host names `desk-tower.local`, `*.example` and `*.example.net`;
- addresses from 192.0.2.0/24, 198.51.100.0/24, 100.64.0.0/10 and 2001:db8::/32, and for networks
  a test has to place, 10.0.0.0/24 and 192.168.1.0/24, 192.168.10.0/24 and 192.168.50.0/24;
- hardware addresses `02:00:00:00:00:01` to `02:00:00:00:00:09`.

`node scripts/check-private-data.mjs` refuses the rest, and also checks private terms from
`LEGION_PRIVATE_TERMS` or `~/.legion-control/private/terms.txt` when configured.

Write a zero-width, bidirectional or byte order mark character as an escape, never as the
character: a reader cannot see it, and `node scripts/check-invisible.mjs` refuses it.

## Commits

One change per commit. The subject is an imperative sentence in sentence case that says what the
change does, such as "Skip a kept revision peer intake refuses when choosing a merge base". A short
body, when the reason is not obvious, says why. Tests come with the change they test. A message
is as public as a file: before pushing, `node scripts/check-private-data.mjs --commits
origin/main..HEAD` reads the messages of the branch's commits.

## Security problems

Report security issues privately through a GitHub security advisory on this repository rather than
opening a public issue.
