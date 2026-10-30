# Contributing

Legion Control is four clients, one agent and one contract, and a change usually touches more than
one of them. This is what a change is held to before it is merged.

## Checks

    ./scripts/check.sh agent      # the agent, the contract, the release tooling and the checkout checks
    ./scripts/check.sh mac
    ./scripts/check.sh android
    ./scripts/check.sh desktop

`./scripts/check.sh` with no argument runs all four on a Mac set up for them. CI runs the same legs on
every platform they support, and a change is not done until CI is green on its exact head: the
Windows legs are the only place the PowerShell dispatcher, the Windows installer and the Windows
process handling run. `docs/testing.md` says what each check covers and which checks only a person
can do before a release.

## The contract

`contract/v3.md`, its schemas, fixtures and vectors are what every client and the agent agree on.
A change to what goes over the wire, or to a rule every client applies, changes the reference in
`contract/tools` first, then regenerates the generated files with `node contract/tools/generate.mjs`,
then ports the rule to each client, whose vector tests read their case counts from
`contract/vectors/index.json`. Note it in `contract/CHANGELOG.md`. Changes only add keys; the
document stays at version 1.

## The repository is public

Write placeholders, never values from a real machine or network:

- user `me`, and homes `/home/me`, `/Users/me` and `C:\Users\me`;
- machines `tower`, `pi`, `nas`, `laptop`, `old-box`, `studio`, `shed` and `tv`;
- host names `desk-tower.local`, `*.example` and `*.example.net`;
- addresses from 192.0.2.0/24, 198.51.100.0/24, 100.64.0.0/10 and 2001:db8::/32, and for networks
  a test has to place, 10.0.0.0/24 and 192.168.1.0/24, 192.168.10.0/24 and 192.168.50.0/24;
- hardware addresses `02:00:00:00:00:01` to `02:00:00:00:00:09`.

`node scripts/check-private-data.mjs` refuses the rest, and with the maintainers' private terms
configured it refuses those too; `docs/testing.md` explains how.

Write a zero-width, bidirectional or byte order mark character as an escape, never as the
character: a reader cannot see it, and `node scripts/check-invisible.mjs` refuses it.

## Commits

One change per commit. The subject is an imperative sentence in sentence case that says what the
change does, such as "Skip a kept revision peer intake refuses when choosing a merge base". A short
body, when the reason is not obvious, says why. Tests come with the change they test.

## Security problems

Report them privately through a security advisory on the repository, as `docs/security.md`
describes, not in a public issue.
