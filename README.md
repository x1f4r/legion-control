# Legion

Legion is a single Rust binary, `legion`, installed on each machine you own. It gives people and AI
coding agents one uniform, self-describing set of tools for the machine it runs on and for every
other machine it can reach over SSH: system state, commands and detached jobs, files and file
transfer, projects, services, power, and the fleet itself.

Every machine is a peer with its own `policy.toml` and append-only audit log. There is no central
server, no open port, and no daemon required: if SSH works, Legion works. The command line, MCP
(`legion mcp`), and an optional loopback HTTP server (`legion serve`) all expose the same tool
registry.

## Install

- **Debian / Ubuntu:** install the `.deb` release package with `sudo apt-get install ./legion_<ver>-1_amd64.deb` (or `arm64`).
- **Arch Linux:** build from `packaging/arch/PKGBUILD` (`legion`) or `packaging/arch/legion-bin/PKGBUILD` (`legion-bin`).
- **Linux and macOS:** run `sh install/legion.sh` (installs to `~/.local/bin/legion`, verified by attestation or Ed25519 signature).
- **Windows:** run `.\install\legion.ps1` in PowerShell (installs to `%LOCALAPPDATA%\legion\legion.exe`).

See [docs/legion/getting-started.md](docs/legion/getting-started.md) for all installer options,
manual verification, and building from source, and [packaging/README.md](packaging/README.md) for
Linux `.deb` and Arch packaging.

## Quick tour

```sh
legion init --name tower                       # create starter node.toml and policy.toml
legion tools                                   # list every built-in and extension tool
legion meta.doctor                             # check local config, tools, and fleet reachability

legion fleet.add name=pi ssh=pi                # add another machine (or use fleet.discover on Tailscale)
legion fleet.ping                              # check which machines answer
legion sys.info --on tower,pi                  # run any tool on one machine, a list, or all
legion cp ~/docs/hello.txt pi:~/docs/hello.txt # transfer files or directories between machines

legion agents add claude --as agent:claude     # register the MCP door with an AI coding agent
```

Output is formatted for reading on a terminal and printed as compact JSON when piped (or with
`--json`).

## Apps and browser door

`legion serve` serves the static web interface (`apps/ui`) on loopback at `/` beside `/v1/*` and
`/mcp`, ready to publish over Tailscale Serve or add to a phone's home screen. Native apps share the
same interface over `legion-link`, talking to paired nodes over SSH or a local `legion rpc` pipe;
see [docs/legion/platforms.md](docs/legion/platforms.md).

## Handbook

Start at [docs/legion/README.md](docs/legion/README.md):

- [Getting started](docs/legion/getting-started.md) — installation, first run, `LEGION_HOME`, adding machines, dual-boot systems, and troubleshooting
- [Permissions](docs/legion/permissions.md) — principals, roles, effects, path confinement, and audit logging
- [AI agents](docs/legion/agents.md) — MCP setup, compact vs. full mode, and extension tools
- [Guides](docs/legion/guides.md) — file transfer, detached jobs, projects, services, power and wake, HTTP door, sync, and updates
- [Tool reference](docs/legion/tools.md) — generated schema reference for every built-in tool
- [Design](docs/legion/design.md) and [Platforms](docs/legion/platforms.md) — architecture, wire protocol, and cross-platform controllers
- [examples/legion/](examples/legion/) — commented example configuration files

---

Versions up to the last Legion Control release (`v1.4.1`) remain available under the repository's
earlier tags.
