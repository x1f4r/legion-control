# Security

What Legion Control trusts, what it does not, and what it asks you to grant it. This page is
written for someone deciding whether to put the agent on a machine, and for someone who has to
recover after something went wrong with a key.

`configuration.md` covers the settings; `operations.md` covers recovery. This page covers the
trust boundary.

## The shape of the thing

The agent opens no listening port. Clients invoke it over SSH, or as a local child process for a
machine bound to this device. Commands return JSON; accepted detached operations and scheduled
maintenance can continue after the client closes.

- **Authentication is SSH's.** Legion Control has no separate server accounts or passwords.
  Authorised keys and the SSH server's policy decide who can connect.
- **Control transport is SSH's.** Downloads, configured probes and wake packets also use their
  respective HTTPS, HTTP(S) or UDP connections. The clients run no network listener.
- **Authorisation is the account's.** The agent has the permissions of its operating-system
  account, including any sudo rules you grant it. A forced dispatcher further limits a key's
  command surface.
- **Polling follows the UI.** Closing a window stops its polling, not work already accepted by
  the agent or the machine's maintenance scheduler.

The threat this is designed against is an ordinary one: a lost phone, a stolen laptop, a mistake
in a config, a corrupted download, a machine that reboots mid-install. It is not designed to
defend a machine against a person who already has your ssh key and shell access to it.

## Keys, per device

Give each client its own key so it can be revoked independently.

- **Android** generates an ed25519 key on first run and keeps the private half wrapped by the
  Android keystore. The public half is shown in the app to paste into `authorized_keys`.
- **Mac, Linux and Windows desktop clients** use the system SSH client and existing SSH keys,
  aliases and configuration. Private bindings can select an identity file; they also recognise
  `~/.ssh/legion-control_ed25519` when present.

Give each device its own line in `authorized_keys` with an administrator-readable comment.
The comment helps you revoke the right key; it is not an authenticated operation-log identity.

### Host keys are pinned and stay pinned

Unknown host keys require explicit fingerprint approval. A changed key is refused until you
verify and explicitly approve it; clients do not silently replace or remove existing pins.
Approvals can add a pin to the client's host-key store, including `known_hosts` on desktop.
A dual-boot machine can have more than one approved key at the same address. Verify each operating
system's fingerprint independently before approving it.

## The restricted dispatcher

The straightforward setup gives a device's key a general shell on the machine. That is more than
Legion Control needs. The dispatcher narrows it to just this agent:

    command="/home/me/.legion-control/bin/dispatch.sh",restrict,no-pty ssh-ed25519 AAAA... phone

`restrict` turns off port forwarding, agent forwarding, X11, PTY allocation and `~/.ssh/rc`;
`no-pty` is redundant with it on current OpenSSH and harmless on older ones. The
`command=` prefix restricts sessions using **that key**, not every login to the account. The
wrapper lives beside the stable launcher, outside the replaceable `agent/` tree; Windows uses
`bin/dispatch.ps1`. It marks the session restricted and invokes the dispatch entry point.

The dispatcher parses the original request with a limited argv grammar. It accepts the configured
interpreter and this agent's entry path or stable launcher alias, checks the command allowlist,
and validates arguments. It never passes the original request to a shell. SSH itself may use the
login shell to launch the fixed forced command; clients quote argv for the dispatcher's grammar.
Set `"restricted": true` in the system's controller configuration before the first request.

A restricted key can inspect status and diagnostics, operate configured services, boot targets
and actions, change policy, manage operations, and publish the shared setup document. It cannot
supply arbitrary executable commands or filesystem paths. All `service-config` verbs, including
`get`, are denied, as are recursive dispatch and internal worker entry points.

Restricted self-update permits `--stdin`, `--check` and `--rollback`. Signed stdin archives are
verified before execution. `--from` and the legacy `--install` bootstrap are denied: those entry
points require administrative access to select filesystem input or run the bootstrap. Normal
unrestricted `--from` updates also require signature verification.

`status` and `doctor` report the dispatcher's session flag. This is descriptive metadata, not a
check of the installed `authorized_keys` policy; inspect the actual key line when auditing access.
The wrapper labels operations `restricted-ssh`, not with a key comment. OpenSSH's optional
`SSH_USER_AUTH` file contains authentication methods and public credentials, not that comment.
See [OpenSSH's ExposeAuthInfo documentation](https://man.openbsd.org/sshd_config#ExposeAuthInfo).

### What a peer can do to the setup

Every device that can reach a machine can also edit and publish the shared setup. That is the same
level of trust it already had: a key in `authorized_keys` can run the agent, and the agent can
update, restart, reboot and suspend the machine. A device that could do all that but was not
allowed to rename a machine in a config file would be a strange boundary to draw.

What the shared document deliberately cannot do:

- **It carries no keys.** No public keys, no `authorized_keys` content, nothing an agent would
  install. A document that could enrol keys would mean one compromised phone could enrol itself, or
  anything else, on every machine in the setup. Authorising a device stays a deliberate act on the
  machine, out of band.
- **It is not the agent's config.** `config set` writes `controller.json`. It cannot touch
  `config.json`, so it cannot add a service, an action or a boot target, and it cannot reach the
  trust key. Shared routing and display settings can change; machine-local command definitions
  cannot.
- **Normal replication requires lineage.** Divergent or stale documents are refused and surfaced.
  Explicit replacement is a separate reviewed action and can bypass that lineage requirement.

Private bindings — identity-file paths, SSH aliases, and which machine this device is — are never
published and never leave the device. That is not only tidiness: an identity file path is a hint about a
filesystem, and there is no reason for it to travel to four machines.

### Waking, and what a helper is trusted with

Asking another machine to send a wake packet is an ordinary configured action, run by id. The
helper does exactly the one thing its own config says that id does. Nothing about the request
carries a command, an address or a hardware address — those are all in the helper's config, put
there by whoever administers that machine.

The native `wol` action sends UDP broadcast datagrams and nothing else. It runs no shell, takes no
arguments from the caller, and cannot be pointed at a different target by the client that triggers
it. Its whole capability is "send this fixed packet to these fixed addresses".

### The line between configuration and code

The configured actions and commands *are* the boundary. A key that can write `config.json` can add
an action, and an action is an argv array. So:

- A restricted key that can also replace `config.json` is not meaningfully restricted. Keep the
  controller document push (`config set`, which writes `controller.json` and never `config.json`)
  separate from agent configuration.
- Anything in `config.json` should be read as code you are installing on the machine, because it
  is. Review a config the way you would review a script.

`service-config get`, `validate --stdin` and `set --stdin` administer the full machine-local
configuration over an unrestricted session. `get` can reveal private configured argv. Validation
and saving require the current hash, preserve concurrent edits through compare-and-swap, and do
not execute the configured commands. Templates and detected application profiles are drafts to
review; automatic maintenance starts disabled. Manual application profiles may provide version
monitoring without an unattended updater.

## Privileges

The agent runs as your user. Boot and power operations, and some configured system services, may
need elevation. Grant the specific commands required by your configuration rather than a shell.

### Linux

    # boot switching, only if you use efi-bootnext
    me ALL=(root) NOPASSWD: /usr/bin/efibootmgr
    # boot switching, only if you use grub-reboot
    me ALL=(root) NOPASSWD: /usr/bin/grub-reboot, /usr/bin/grub-editenv list
    # rebooting and suspending
    me ALL=(root) NOPASSWD: /usr/bin/systemctl reboot, /usr/bin/systemctl suspend
    # only if a service uses the systemd-system process type
    me ALL=(root) NOPASSWD: /usr/bin/systemctl start <unit>, /usr/bin/systemctl stop <unit>, /usr/bin/systemctl is-active <unit>

Grant only the lines for the methods you actually configured. `doctor` tells you which ones this
machine's config needs.

The `efibootmgr` and `grub-reboot` examples above permit all arguments to those tools. For a
narrower policy, constrain them to the configured boot numbers or entries and required read-only
queries. `doctor` and command preflights help diagnose missing permissions, but do not prove that
your sudo policy grants only those operations.

**No sudo rule for a shell.** The delayed reboot used to be `sudo -n sh -c 'sleep 3; systemctl
reboot'`, which is a rule that grants root a shell with arbitrary arguments — a root shell wearing
a hat, and not what the documentation claimed was needed. The delay now happens unprivileged: the
agent detaches an ordinary process that sleeps and then calls the narrow `sudo -n systemctl
reboot`. Nothing needs `sh` with sudo.

If you would rather grant one thing than several, write a helper that does exactly one thing, make
it root-owned and not writable by your user, and point `boot.rebootHelper` at it:

    me ALL=(root) NOPASSWD: /usr/local/libexec/legion-reboot

A helper is only an improvement if it takes no arguments it then passes on. A helper that takes a
command to run is the shell rule again.

### Windows

Boot switching with `bcdedit` requires elevated rights. Treat an administrator SSH session as
administrative access, even when a particular key is forced through the dispatcher. The standard
Windows OpenSSH administrator configuration uses
`C:\ProgramData\ssh\administrators_authorized_keys`; check the server's actual `sshd_config` and
protect the selected key file's ACL. Prefer a standard account when the configured operations do
not require elevation. Its ability to manage services or power depends on the machine's policy.

### macOS

Sleeping wants nothing: `pmset sleepnow` is what the Apple menu does and needs no elevation.
Rebooting is the one exception —

    me ALL=(root) NOPASSWD: /sbin/shutdown -r +1

— and only if you want to reboot this Mac remotely. Without it the reboot is refused with that
sentence rather than failing obscurely.

Everything else here belongs to your user. The scheduled job is a launchd *user* agent, and running
the installer under sudo breaks it by leaving root-owned files the agent cannot write.

## Release signing

Bundled agent updates and downloaded application updates require signatures. Their trust is
anchored on one Ed25519 public key committed to this repository:

    contract/release-public-key.pem

Clients and the agent embed that key. Update artifacts cannot supply a replacement key or bypass
verification. An administrator can separately install an explicitly selected unsigned source
checkout with the platform installer; that is not a publisher-verified update.

### The private key

    ~/.legion-control/release-signing.key

Generated once, by the maintainer, on the machine that cuts releases. It is never in the
repository, never in a build, never in an artifact, and `.gitignore` is not what keeps it out —
it lives outside the tree entirely.

    node scripts/release-integrity.mjs keygen ~/.legion-control/release-signing.key contract/release-public-key.pem

The generator refuses to overwrite either file if one already exists. The private key is written
`0600`; a newly created parent directory is `0700`. Existing directory permissions are not changed.
Back it up wherever you keep your other signing material; losing it is discussed below.

### What gets signed

Two manifests, each with a detached signature over its exact bytes.

**The release manifest**, `Legion-Control-manifest.json`, lists every artifact of a release with
its exact basename, its sha256 and its size:

```json
{
  "schema": 1,
  "version": "1.3.0",
  "agentVersion": "3.0.0",
  "artifacts": [
    { "name": "Legion-Control-macos-arm64.zip",  "sha256": "…", "size": 2184887 },
    { "name": "Legion-Control-android-arm64.apk", "sha256": "…", "size": 29994567 },
    { "name": "Legion-Control-linux-x64.tar.gz",  "sha256": "…", "size": 0 },
    { "name": "Legion-Control-windows-x64.zip",   "sha256": "…", "size": 0 },
    { "name": "legionctl-agent-3.0.0.tgz",        "sha256": "…", "size": 0 }
  ]
}
```

`scripts/sign-release.mjs X.Y.Z` writes and signs it, then verifies its own output against the
public key before exiting. A client selects **the exact asset for its platform by name**, never
"the first zip", and checks size and digest against the manifest before it unpacks anything.

**The agent manifest**, inside the agent tarball at `agent/MANIFEST.json`, lists every file in the
tree with its sha256 and size, and carries its own detached signature beside it.
`scripts/package-agent.mjs` builds the tarball, writes and signs both the internal manifest and a
release-shaped manifest for the agent artifact alone, and verifies everything it just produced
before it copies anything into `dist/`.

The signature format is deliberately boring: raw Ed25519 over the exact manifest bytes, base64 with
a trailing newline. A release manifest larger than 256 KiB is refused before JSON parsing;
a non-object manifest is also refused.

### Installing an agent onto a machine

The clients bundle the agent tarball and manifest of the release they came from, so upgrading a
machine does not need a download at all. The install action:

1. verifies the bundled release manifest's signature and the archive's exact signed size and
   SHA-256 before uploading anything;
2. streams the archive to a v3 agent's `self-update --stdin`; an unrestricted legacy bootstrap
   instead stages the verified archive under `<base>/incoming/agent` and runs its `--install`
   entry point only for a recognised installation base;
3. has the existing v3 agent or the authenticated bootstrap verify the internal agent manifest,
   signature and exact file inventory before staged self-testing and promotion;
4. runs the staged self-test and checks the expected agent and contract versions;
5. publishes stable launch helpers under `<base>/bin`, then swaps the trees under the operation
   mutex using a recovery journal, retaining the previous tree.

The archive verifier rejects unsafe or ambiguous paths, links, duplicate entries, and files whose
inventory, size or hash differs from the signed manifest. Stdin uploads are bounded before the
operation mutex is acquired. Existing custom launcher argv are preserved; migrating a legacy
entry path to the stable launcher is explicit.

Rollback verifies the retained tree against the publisher key before staging and swapping it. An
unsigned legacy tree is not a normal rollback target. For interrupted swaps only, the stable
launcher can restore the exact old unsigned tree against a local pre-swap inventory whose hash is
anchored in the journal. This recovery attestation is not a publisher signature and cannot verify
a different unsigned candidate. Recovery fails closed if neither signature nor the anchored
inventory verifies the old tree.

A build without a valid bundled artifact explains why installation is unavailable. Clients and
the agent must not report an unverified update as signature-verified success.

### One key, and no way to name another

There is no runtime configuration or environment setting that adds, replaces or bypasses the
release trust key. It is embedded in application code and the agent's trust module, not loaded
from a key placed beside a downloaded artifact. The Node agent's installed code is still ordinary
local files: someone who can replace that code already controls the agent.

The shared `config set` command writes `controller.json`, not the machine's `config.json`, and
cannot change release trust. Administrative service configuration can define commands, but also
cannot nominate a signer. A downloaded artifact must be authorised by the installed trust key,
not by a key supplied with that artifact.

### The Mac app updating itself

Same rules, applied to a bundle instead of a tarball. It downloads the exact named asset for its
platform, checks it against the signed manifest, unpacks and verifies in a temporary directory,
retains the current bundle as `Legion Control.previous.app`, and swaps through a helper after a
bounded wait for the old process to exit. If the new build does not write its launch marker within
45 seconds, the helper attempts to restore the previous bundle. Staging and verification precede
the swap; filesystem or process failures can still require recovery.

The other desktop clients likewise verify the signed artifact and use an update helper with a
retained previous installation.

## Losing the signing key

This is the failure mode worth planning for, because the recovery is bad and knowing that in
advance is the whole mitigation.

**The key is compromised but you still have it.** Replace the signing key and the embedded trust
anchors through an independently verified transition. Changing only
`contract/release-public-key.pem` is insufficient: clients, the agent and its stable launcher must
agree, including verification after the next launch and during recovery. This repository does not
provide a general key-rotation protocol. Do not assume an old-key-signed build containing a new key
can complete that transition; verify every step or replace installations out of band.

**The key is lost outright.** There is no recovery path that uses the software. Nothing in the
field will accept anything signed by a key it does not have, and that is the property the signing
exists to provide. Every installed client and every agent must be replaced by hand — rebuilt from
source by the person who owns the machine, or reinstalled from a build they verified themselves out
of band — before automatic updates work again. Publishing an unsigned release, or a release signed
by a key announced in the release itself, would defeat the entire mechanism, so the project does
not offer it.

Back the key up somewhere that survives losing the machine.

## Diagnostics and redaction

`legionctl bundle` collects doctor, status, history, recent log lines and configuration into one
object. It masks configured command arguments after the executable, recognised credential fields
and identity-file values. Known configured values, including credentials in URLs, are also removed
from their exact echoes in the exported doctor, status, history and log evidence. Standalone
`doctor` applies the same configured-value sanitization; ordinary action and log views retain
their output.

This cannot identify every secret originating in command output or a transformed value. Review
the whole bundle before sharing it. Prefer credentials read from a protected file over literal
argv values, which can also appear in process listings and command output.

## What is deliberately not here

- **No remote shell.** There is no "run arbitrary command" verb. The `run` command executes an
  action you defined in the config, by id. This is also why waking a machine through a helper is
  built on ordinary configured actions: the helper does the one thing its own config says it can
  do, and asking it to wake something grants nothing new.
- **No central credential service.** SSH identities remain on their devices; Android stores its
  private key encrypted with a keystore-backed key. Private client settings can contain secrets
  and require the same care as other local credentials.
- **No per-service user roles.** Restricted keys share the dispatcher's allowed capabilities;
  unrestricted sessions also have administrative configuration access. This is a personal,
  trusted-device tool, not a general multi-user authorisation system.
- **No discovery.** Nothing announces itself on your network and no client listens for anything.
  The apps promise to open no ports, and mDNS would be a listener. Machines are the ones you wrote
  down.
- **No external analytics reporting.** Optional `status.metrics` readings come only from explicitly
  configured local telemetry probes and are returned to your clients. Probe commands themselves
  have the account's permissions and must be reviewed like any other configured command.

## Reporting something

If you find a problem with any of the above, open an issue with the reproduction and please do not
include a bundle until you have read it.
