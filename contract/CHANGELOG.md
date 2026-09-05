# Contract changelog

The wire contract between the clients and the agent. Within a major contract
number, keys are only ever added — never renamed, never retyped, never removed.
A client decodes with every field optional.

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
