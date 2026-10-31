# Contract changelog

The wire contract between the clients and the agent. Within a major contract
number, keys are only ever added — never renamed, never retyped, never removed.
A client decodes with every field optional.

## 3 — agent 3.1.1, clients 1.4.1

### Fixed (schemas)

- The busy schema names what the `t3-sqlite` probe has always sent:
  `abandonedTurns`, the count of running rows retired by outside evidence;
  `abandoned` as the `disposition` of such a row; and `path`, the database the
  probe read or looked for (section 6). Every `t3-sqlite` answer carries
  `abandonedTurns`, so none of them passed the schema before.

---

## 3 — agent 3.1.0, clients 1.4.0

Contract stays 3 and the setup document stays `"version": 1`. Every reply
change only adds keys or brings a reply in line with the fixtures, and a client
recognises each added key by its presence, never by comparing versions (v3.md
section 1, "Additions in agent 3.1.0"). The setup document's guard grammars now
refuse values that ssh, a shell or known_hosts would read as syntax, so a few
documents the 3.0 schema allowed are refused (see Changed).

### Added

- **`outcome`** on every reply to a mutating invocation, on every operation
  record (`op.outcome`, and `result.outcome` once finished) and on every
  summary: `succeeded`, `failed`, `deferred`, `pending` or `unknown`, the field
  a client decides on (section 5.6). When it is absent a client reduces it with
  the one function of section 5.6, `contract/tools/outcome.mjs`.
- **`raw` on `config`**: the stored document as text, exactly the canonical
  bytes `hash` covers, null exactly when `controller` is. `bytes` is its UTF-8
  length. Clients hash and adopt `raw` and never re-serialise `controller`. A
  `config` reply is at most 4 MiB of stdout (section 4).
- **The version floor.** `status.agent.floor` and `floor` on every
  `self-update` reply, `null` when the agent can state none (an unreadable
  floor file, or none recorded under a 2.x or 3.0.x live tree);
  `self-update --allow-downgrade`. A signed agent declares
  `legionControl.agentFloor`. A restricted session only installs forward or rolls
  back to a tree at or above the floor, and an unrestricted one needs the flag
  to go below the live version or the floor (section 13).
- `status.agent.signed`: whether the running tree carries its signed manifest.
- Doctor checks `agent.floor`, `agent.signed`, `files.private`, `scheduler.timeLimit` and
  `ssh.shell`. Agent 3.1.0 sent `scheduler.timeLimit` before the schema named it, wherever it
  could read the scheduler's time limit.
- `problems: [{path, message}]` on a `config-invalid` refusal of `config set`.
- `config set` refusal codes `lock-held` and `internal`, which the agent
  already sent and the schema did not allow.
- `interrupted` among the actions of `boot` and `sleep` (section 5.3), which
  recovery already wrote on a record whose owner was gone, as section 5.5 says
  of every kind, and a replay of that record answered with. A reboot judged
  after a return from another system is now one too, rather than `failed`.
- The guard grammars in `common.schema.json`: `sshHostGuard`, `sshUser`,
  `agentArgItem`, `githubRepo` and `launchPath`.
- **`restricted` and `launchChanges` on `config set`.** A restricted session's
  push that would introduce a launch value the machine does not hold, or replace
  a held copy the machine cannot read, is refused with `restricted` and
  `current`, plus `launchChanges`, the paths of the refused values and never the
  values, when the held copy could be read (section 9.4).
- **The approval record**: the launch values a device approved, kept beside its
  applied setup in `approved-launch.json` (Android: the `approved-launch`
  preference), private like the bindings and never published or merged (section
  11).
- The `launch` vectors: what waits for review, what a device runs meanwhile, and
  what a restricted push gets, case by case (`contract/vectors/launch.json`).

### Changed

- **The agent checks a document before `config set` stores it** (section 9.4):
  size, UTF-8 and parse on the canonical bytes, then a repeated member name,
  then the `controller` block and the flags, the acceptance table, and the
  document's structure, refused as `config-invalid` with `problems`. Agents
  before 3.1.0 stored whatever parsed, so clients check before they publish.
- **Replies to a restricted session are redacted**: configured command
  arguments, credential-named values and credentials in configured URLs become
  `<redacted>` in free text. Identifiers, enum values, hashes, paths and the
  setup document are never altered. A failed command is named by its
  executable in every session, and an unparseable `config.json` by line and
  column.
- `controller-document.schema.json` holds hosts, users, argv items and the
  update repository to the guard grammars, types the deprecated `machine.ssh`,
  and allows an empty `agent`, which means the system is not driven. A document
  the 3.0 schema allowed, one with a non-ASCII or `$`-suffixed login name for
  example, is now refused by peer intake and by `config set`, and agent 3.1.0
  refuses some pushes a 3.0.x agent stored with `config-invalid` or
  `restricted`.
- **The agent reads launch values in restricted sessions.** It compares a pushed
  document's launch values with the ones the held document carries, by the rules
  of section 9.9; unrestricted sessions are never refused for them.
- Rule 5 of section 1 now points at section 15: an argv is serialised for its
  system's grammar exactly as that section specifies, and a system restricted on
  the route uses the dispatcher grammar on every platform. It also states how
  every pattern of the contract matches: the whole input, ASCII classes only,
  and U+000A or U+000D refused first.
- The `cycle` and `self-update` schemas allow the action `already-running` and
  the `detached` copied onto a replay, which is how agent 3.1.0 answers a
  replay of a running record (section 5.5).
- A cycle child deferred by policy may have `opId: null`; only a deferral may.
  3.0.x agents already sent it, and the schema now allows it.
- A cycle's `queued` and `recovered` entries may have `service: null`. A
  queued or recovered boot, sleep, run or cycle names no service, and the
  schema refused every reply that carried one.
- **Process evidence.** A process probe whose `error` is set or whose `state` is
  `unknown` is not evidence that the process stopped: no running turn is
  abandoned and the stop guard holds (section 6). A recovery obligation clears
  only on a positive running reading or a successful start; a failed or
  unreadable probe keeps it and says why (section 5.5).

### Fixed (agent replies)

Where the fixtures, the schemas and this contract agreed and the agent did
not, the agent now answers as they say.

- `op` with an unknown id answers `not-configured` with `op: null`, not
  `bad-argument` with extra keys. A client treats a present `op: null` as "no
  record" under either code.
- `logs --op` with an unknown id answers `not-configured`, naming `--op`.
- `cancel` with an unknown id answers `not-configured` in the refusal shape,
  not `action: "failed"`.
- `busy` on a machine with no services answers `monitored: true`,
  `evidence: "none"`, both counts 0.
- A service whose `busy.type` is `none` is monitored.
- A replay of a running operation answers `action: "already-running"` with
  `ok: true`, `replayed: true` and the record's `detached`, never `accepted`.
- A lock `conflict` answers `ok: true`.
- A finished record's `result.ok` is true exactly when its `reasonCode` is
  null. A failed cycle in which every service's ending is known carries the
  code of its first failed service.
- A cycle that ran a service whose ending is unknown (an interrupted update, one
  that ran out of time, or one that may have changed the machine before it
  failed) states `outcome: "unknown"` and takes the reason code of the first
  such service, even when one before it failed with a known ending, rather than
  `cycled` or a `failed` that reads as a known state. Its message counts such
  services apart from the ones that failed.
- A queued request the agent cancelled because it could not safely replace the
  one before it (`cancelled` with `internal`) reduces to `unknown`, not
  `succeeded`: section 5.6 reads `cancelled` by its reason code, with null
  `succeeded`, `cancelled` `deferred`, `internal` `unknown` and any other
  `failed`.
- `cancel` of a running operation answers `conflict` with
  `reasonCode: "already-running"`, not `operation-in-progress`.
- `--detach` replies carry every key of their command's shape, and a fresh
  record has a phase.
- `cycle --dry-run` answers `skipped`.
- An `--op` id reused with another intent is refused with `bad-argument`
  naming `--op`, never replayed and never reported as a lock `conflict`.
- A `config set` refused by the operation lock exits 1, with outcome
  `deferred`.
- `update` answers `failed` with `latest-unknown` when the newest version
  cannot be resolved, and `failed` with `postcondition-failed` when nothing
  could confirm an update, never `noop`, which reads as done. An app whose
  staged build used up its attempts answers `failed` with
  `apply-attempts-exhausted`, and one whose staged build waits for the app to
  quit answers `deferred` with `app-closed`.
- A mutation refused because `config.json` cannot be used is a refusal, with
  `command`, `argument` and `problems` and no `action`.
- `policy set` refusal problems carry `level` and `fix`, like every other
  `problems` list.
- A `config set` refused with `controller-conflict` always says `divergent`.
- Every doctor check that is not `ok` offers a fix.
- `bundle` redacts configured arguments from free text only. Identifiers, enum
  values, versions and timestamps are kept, so an argument such as `restart` no
  longer turns an operation's kind into `re<redacted>`.

### Client rules

- Decide on `outcome`; reduce it with section 5.6 when it is absent or a value
  the client does not know. `pending` is polled with `op ID --wait 20`, the
  terminal outcomes stop polling, and `unknown` is never resent automatically.
- Hash and adopt `config.raw`. Without it, rebuild two-space JSON in received
  key order and adopt only on a matching hash, otherwise decline.
- Before publishing, pasting, importing or adopting a document, refuse it for the
  first of `too-large`, `invalid-utf8`, `empty` and `not-a-json-object`, the
  last two decided on the canonical bytes (section 12).
- `process.running` is evidence only when `process.error` is null; otherwise the
  client shows the process as unknown (section 8).
- Never send `--allow-downgrade` to an agent whose status carries no `floor`,
  and only after a person decided to go back.
- **Launch values need local approval** (section 9.9). A system's
  `{agent, grammar}`, `machine.ssh.identityFile`, `appUpdates.githubRepo` and
  `local` are applied with the document and run only once this device approved
  them; until then the value last approved at that place runs (the built-in
  repository for `githubRepo`), or nothing, and a recorded value that could not
  run itself counts as none. Approve, a local edit, enrolment after a summary of
  every launch value, and the first start of a 1.4 client approve; replication
  never does. A setup file whose `controller` block is missing or has no `id`,
  written on a device that applies no setup yet, is a local edit of nothing and
  approves what it holds; put over an applied setup it approves nothing. The
  launcher migration under the same base and grammar is accepted without asking.
- **Command lines and retry safety** (section 15). The grammar is `dispatch`
  when the system is restricted on the route, else its `shell`, else `posix`, or
  `cmd` on Windows; it is never inferred and a line is never retried in another
  grammar. Each grammar has one exact serialisation and refuses what it cannot
  carry. ssh's own log, never remote stderr, shows that a command was never
  sent; an unexplained exit 255 is `linkLost`, `unknown`. A mutation moves to
  another route or system only after a failure that is `never`. `status` sends
  `--budget-ms` of `min(20000, remaining − 5000)`, at least 1000. Hosts, users
  and ports are held to the dial grammar before they are dialled, scanned or
  pinned, and ssh and ssh-keyscan get `--` before the host.
- **Document acceptance is normative** (section 9.7). Every client accepts
  exactly the documents `contract/tools/document-rules.mjs` accepts, under peer,
  local or applied intake. What a client cannot use is a warning with a code,
  never a refusal, and the document this device already applies is never refused
  while section 12 accepts its bytes. `wake.helper` is a helper even beside an
  empty `wake.helpers`, where `validate.mjs` used to skip it, so the alias is
  held to `helper-self`, `helper-unknown` and `helper-cycle`.
- **Client rules for setup sync, acceptance and merge are normative** (sections
  9.5–9.8), with one reference, `contract/tools/setup-sync.mjs`, and the
  `setup-reconcile` and `setup-merge` vectors. A status is decided by eleven
  ordered rules, and automatic writes follow strict descent only. Fetched bytes
  are adopted only when they hash to what the decision expected, and only as a
  compare-and-swap against the head the decision was made from, so an edit saved
  during the read is never overwritten; Take theirs, Keep mine and Merge do the
  same. One setup request per machine at a time, a 60 s back-off per applied
  hash, no push during a running mutation, and a push refused with `restricted`
  or `config-invalid` is held until the hash changes or a person asks. A merge
  goes entry by entry: it asks about a deletion against an edit, keeps endpoints
  without an id and unknown keys, inserts what theirs added after its
  predecessor, and never rewrites a number. Theirs' n-th endpoint without an id
  stands for the result's n-th when it places what theirs added. A top-level key
  that begins with `"`, `controller.`, `sites[` or `machines[` is an entry
  written inside double quotes, so a difference's `entry` may be quoted. A
  device whose applied document breaks only `unknown-site` or a `helper-` rule
  still merges, and only a result section 9.7 accepts is written. The base is
  the newest revision both lineages share whose kept bytes hash to it and which
  peer intake accepts; a kept revision peer intake refuses is skipped like one
  that is not kept, and with no base every difference is asked about. A device
  whose applied document has a section 9.7 error never sends `--replace`.
- **A client older than its agent** keeps using the highest contract it
  implements and says so once (section 1, rule 9). Nothing on the wire changes.
- **Sites and wake are normative** (sections 10.1 and 10.2), with one
  reference, `contract/tools/wake-plan.mjs`, and the `wake` vectors. None of the
  rules below changes the wire or adds a document key.
- **LAN prefixes** are dotted (one to three octets and an optional trailing dot)
  or CIDR (`/8` to `/32`, no host bits), matched on octet or bit boundaries, so
  `192.168.1` no longer holds 192.168.10.7. Anything else never matches and is a
  warning.
- **Placement** counts only addresses on broadcast-capable interfaces; on
  Android never a VPN network or one blocked for the app. `currentSite` counts
  only while one of its site's prefixes matches or the site has none. Exactly one
  matching site is effective; several are ambiguous and none is picked.
- **Direct wake** needs a `wake.mac` and the target on this device's network;
  otherwise the direct step is skipped as `no-mac`, `ambiguous`, `unplaced` or
  `elsewhere`. It sends to `wake.broadcast`, else the site's `broadcast`, else
  the directed broadcast of the placing address when its prefix length is known
  and at most 30, else 255.255.255.255. Only dotted IPv4 counts and names are
  never resolved. Ports are 1–65535, else 9 and 7, and every address and port
  gets three packets.
- **Readiness** is the authenticated status poll, first at 5 s and then at
  intervals growing 1.6× to at most 20 s, for 120 s after each send. A step that
  sent nothing does not wait.
- **Helpers** are walked as `helpers`, else `[helper]`, and an entry is skipped
  as `missing`, `self`, `duplicate`, `asleep`, `unusable`, `no-action` or
  `held`, checked in that order. After `ran` the client waits for the target
  and moves on only if it does not answer.
- **Wake holds.** A helper action not declared `wol` that ends ambiguously
  becomes a hold on its target, kept in this device's operation history with
  `wakeTarget`. While a hold exists no non-`wol` helper runs for that target. It
  is released only by a finished record, an answer that the operation is
  unknown, the target answering, the helper leaving the setup, or a person
  taking one of the two ways past it.
- **Going past a hold** has two forms, the same on every client. Try the other
  helpers asks only the helpers the holds kept back, never a held command again,
  and is offered only while one of them could be asked. With none, Run the held
  command again is offered instead, after a warning that the command may then
  run twice beside the first.
- **Helpers are never woken automatically.** "Wake it first" is offered only for
  a helper skipped as asleep that has a wake block of its own.
- **Android** tracks every network that is not a VPN, with an address book per
  network, and sends magic packets from the network holding the placing
  address. The off-contract wake verb probe, the TCP readiness probe and the
  unused readiness helpers are gone; no 1.4 client reads `wake.probe.host`.

### Conformance

- `contract/vectors/index.json` lists the vector families, the hash vectors
  included, with the cases each consumer runs per group: `hash`,
  `identifiers`, `outcome`, `transport`, `setup-acceptance`, `launch`,
  `setup-reconcile`, `setup-merge` and `wake`. Each family has a schema, a
  zero-dependency reference in `contract/tools`, and cases whose expectations
  are produced by the reference and stated by hand; generation refuses a case
  where the two disagree.
- `node contract/tools/generate.mjs [--check]` is the one entry point for every
  generated file. `--check` fails on a stale, missing or extra byte.
- The fixture index carries `outcome`, the `exitCode` rule 7 gives, and
  `config meta` as a command of its own.
- Clients walk the fixture index and the vector index; a missing corpus is a
  failure, never a skip.
- The agent decides `config set` acceptance on the canonical bytes, so a byte
  order mark or surrounding whitespace no longer makes a document unparseable.
  The hash vector `double-bom` pins that exactly one leading byte order mark
  is stripped.
- `contract/bootstrap/prepare.cjs` and `install.cjs` are the two scripts that
  upgrade a 2.x agent from a locally verified archive. The Mac and Android
  copies are compared with them byte for byte; `install.cjs` checks the upload's
  size and SHA-256 before tar reads it.
- `fixtures/controller-document.wake-defaults.json` is the setup every wake
  plan vector runs against, `WAKE_DEFAULTS` in `contract/tools/documents.mjs`.
- `contract/test/transport-shells.test.mjs` runs serialised lines through sh,
  dash, bash, zsh, ksh and fish, and on Windows through cmd.exe, Windows
  PowerShell and pwsh, and checks that node receives exactly the argv. A shell
  that is not installed is skipped with the reason. Run through pwsh, it showed
  that PowerShell takes `--%` as its stop-parsing token even quoted, so the
  powershell grammar refuses that element (`stop-parsing`).

---

## 3 — agent 3.0.0, clients 1.3.0

The first contract with durable operations, a real policy model and a setup that
several devices can edit.

### Added

- `contract: 3` and `system` on every reply. This is the only feature gate;
  there is no capability negotiation.
- A closed `reasonCode` enum of 35 codes. Every "nothing happened" now says
  which of the ten different nothings it was.
- **Operations.** Durable records under `<base>/ops/<id>.json` with a kind, a
  mode, a state, a phase, progress, a log and a result. `op`, `history`, `logs`
  and `cancel` read them. `--op ID` makes a mutation idempotent, so a client
  that lost its link retries with the same id and can never start the work
  twice; `--detach` answers `accepted` and the client polls.
- **One machine-wide operation lock.** A second mutation answers `conflict` and
  names what holds it. `--force` never breaks it.
- **Recovery.** A record whose worker died is finished as `interrupted`, and an
  interrupted update brings the service back up and rolls back when the
  installed version is neither `from` nor `to`.
- **The when-idle queue.** `--when-idle --expires DUR`, run by `cycle`, listed
  in `status.operations.queued`, cancellable.
- **The policy model.** `updates.automatic`, `pauseUntil` and
  `maintenanceWindows`, per machine and per service with inheritance, read and
  written through `policy` and `policy set`. Scheduled eligibility, manual
  requests and force are three separate things for the first time.
- **`cycle`**, the scheduled pass over every eligible service, with a
  `--dry-run` that says what would happen and why not.
- **`doctor`, `bundle`.** Named checks with a fix in the imperative, and one
  reply carrying everything needed to diagnose a machine.
- **Bounded `status`** with `timing.partial`, per-probe timeouts, and
  reachability split into five separate things that used to be one boolean.
- **Signed `self-update`** with a staged tree, a self-test before the swap and a
  kept previous tree. Nothing is extracted before the signature is verified.
- **Stable recovery launcher.** Signed helpers are installed outside the replaceable
  agent tree before a swap. A journal and anchored local pre-swap inventory preserve
  crash recovery for a legacy installation without treating it as publisher-signed.
  Existing explicit agent argv stays unchanged until a reviewed migration.
- **Configured telemetry.** Optional `status.metrics`, at most eight explicit
  agent-local probes under the shared status deadline. Missing readings remain null;
  no probes means no collection. `configured-telemetry` documents support.
- **Administrative service editor.** `service-config get`, `validate --stdin`, and
  `set --stdin` provide a full local document, templates, validation, a change preview
  and compare-and-swap saving. All are denied to restricted sessions. Saving runs no
  service or setup command; `service-config-admin` documents support.
- **Optional application profiles.** Administrative reads can list detected apps and
  the supported update method or manual/unavailable state. Drafts leave automatic
  updates disabled; availability is specific to the platform and installed software.
- **The restricted dispatcher**, which parses a limited quoted argv grammar and
  never hands anything to a shell.
- **Setup identity and lineage** — see the amendment below.
- **Sites, wake helpers and the `wol` action** — see the amendment below.

### Changed

- Service/action/boot identifiers and setup identifiers now have separate schema
  definitions matching the server's actual validation. Generic argv tokens and shared
  machine/site identifiers retain their own grammar.
- Agent self-tests check their nested outcome and exact signed version/contract.
  Uploads are bounded before the operation lock. Install and rollback operation ids
  bind distinct immutable targets, and rollback retries replay the original target.
- `busy` gained `monitored` and `evidence`. A service with no busy probe is now
  `unmonitored` and **blocks**, where 2.x silently read it as idle. This is the
  one behaviour change that will surprise an existing configuration, and it is
  deliberate: the gate's central promise had a silent exception in it.
- A running turn no longer ages out on its timestamp alone.
- A `command` service update now has a postcondition. An exit code is not
  evidence that a version changed.
- Canonical setup bytes and one hash rule, defined once in
  `contract/tools/canonical.mjs` and proved by `contract/hash-vectors.json`. 2.x
  hashed the stored form on one side and the raw file on the other, so a
  document with no final newline never agreed with itself.

### Kept for 2.x clients, removed in contract 4

`status.t3`, `status.pendingRestart`, `status.lastUpdate`, `status.connect`,
`status.autoUpdate`, and `run`'s top-level `id`.

### Not real

An early draft of the client work invented a "2.x dialect" — `--operation`,
`operation get`, `log`, `queue`, `pause`, `window`, `update --all`. No agent
ever spoke it. Agent 2.1.0 has no durable operations, no policy, no queue and no
doctor. Do not implement, send or decode any of it.

---

## 3, network peer amendment

Applied before contract 3 shipped, so it is part of contract 3 rather than a
successor to it. Additive throughout; no key is renamed, retyped or removed, and
the document `version` stays 1 so a 1.2 client still loads a 1.3 document.

### Added

- `controller.lineage`, up to 32 ancestor hashes, newest first. **The agent now
  decides acceptance by ancestry rather than by revision order.** A push is
  taken when the machine's hash is the pushed hash (`noop`) or appears in the
  pushed lineage (fast-forward); a pushed hash found in the held lineage means
  the pusher is behind; anything else is divergence and stops at a person.
- `config set` replies gained `action` (`stored` | `noop` | `replaced`) and
  `divergent`. `controllerMeta` gained `lineage` and `device`.
- `sites[]`, `machine.site`, `machine.alwaysOn` and `wake.helpers[]` in the
  shared document. `wake.helper` and `wake.lanPrefix` stay as compatibility
  aliases, and an editor writing `helpers` must also write `helper`.
- Agent actions gained `kind`, and a `wol` action the agent sends itself, so a
  Pi or a NAS can be a wake helper without a third-party binary.
- `contract/schemas/controller-document.schema.json` and
  `contract/schemas/bindings.schema.json`: the shared document and the private
  per-device bindings now have a written shape.
- `policy` on a machine carries `services[]`, so a client draws the maintenance
  screen from one call instead of one call per service.

### Changed

- **There is no authority device.** Every client edits and publishes, the phone
  included. `controller.id` is now the **setup** id, shared by every peer; it
  used to identify a device. The Mac's authority sidecar and the desktop's
  `IsSetupAuthority` are gone, and a leftover sidecar is ignored rather than
  migrated. Android's `device-<android id>` identity is gone: a pasted document
  is an ordinary setup with a fresh id.
- Private state — which machine this device is, key paths, ssh aliases, local
  agent argv, known-hosts location — moved out of the shared document into
  per-device bindings. The shared `machine.ssh.identityFile` and `local` are
  deprecated and still read, and are never stripped automatically, because
  stripping them would change the bytes and so the hash on one device only.
- Wake became an ordered failover list with explicit reasons when nothing can
  reach the target. Helpers are never woken automatically and never cascade.
- `logs` lines are `{ at, line }` with an optional `command`.

### Why revision numbers were not enough

Two peers editing offline both produce revision 6 from revision 5, and neither
descends from the other. A fast-forward rule based on revision order accepts
whichever arrives second and destroys the first, silently. Only ancestry can
tell "newer" from "different", which is why the lineage exists and why the cap
of 32 fails toward a question rather than toward a loss.

---

## 2 — agent 2.1.0, clients 1.2.0

Several services per machine (`services[]`, `--service`), boot targets,
configured actions, the shared setup document with a hash, and the busy gate.

## 1

One service, reported under `t3`.
