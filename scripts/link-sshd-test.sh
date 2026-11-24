#!/usr/bin/env bash
# End-to-end test of `legion-link`'s in-process SSH client (`russh`) against a throwaway loopback
# `sshd` forcing `legion gate --as phone` with a throwaway `LEGION_HOME`.
#
# Never touches ~/.ssh or system sshd configuration; all temporary keys, configs, and state live
# under ${XDG_CACHE_HOME:-$HOME/.cache} and are removed on exit.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"

LEGION_BIN="${LEGION_BIN:-$ROOT/target/release/legion}"
LINK_BIN="${LINK_BIN:-$ROOT/target/debug/examples/link}"

if [[ ! -x "$LEGION_BIN" ]]; then
  echo "Building release legion binary..."
  cargo build --release -p legion-cli
fi

echo "Building legion-link example binary..."
cargo build -p legion-link --features ssh --example link

SSHD_BIN="$(command -v sshd || true)"
if [[ -z "$SSHD_BIN" && -x /usr/sbin/sshd ]]; then
  SSHD_BIN="/usr/sbin/sshd"
fi
if [[ -z "$SSHD_BIN" ]]; then
  echo "link-sshd-test: sshd not found on PATH or /usr/sbin/sshd" >&2
  exit 1
fi

CACHE_BASE="${XDG_CACHE_HOME:-$HOME/.cache}"
mkdir -p "$CACHE_BASE"
WORK="$(mktemp -d "$CACHE_BASE/legion-link-sshd-XXXXXX")"
chmod 700 "$WORK"

cleanup() {
  if [[ -f "$WORK/sshd.pid" ]]; then
    local pid
    pid="$(cat "$WORK/sshd.pid" 2>/dev/null || true)"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

PORT="$(node -e '
const net = require("node:net");
const srv = net.createServer();
srv.listen(0, "127.0.0.1", () => {
  console.log(srv.address().port);
  srv.close();
});
')"

USER_NAME="$(id -un)"
FAR_HOME="$WORK/far"
mkdir -p "$FAR_HOME/config" "$FAR_HOME/state"

cat > "$FAR_HOME/config/node.toml" <<'EOF'
name = "tower"
EOF

cat > "$FAR_HOME/config/policy.toml" <<'EOF'
default = "owner"

[principals.phone]
role = "observer"

[roles.observer]
allow = ["effect:read"]
EOF

ssh-keygen -q -t ed25519 -N '' -C "legion-link-host" -f "$WORK/host_key"
chmod 600 "$WORK/host_key"
HOST_PUB="$(cat "$WORK/host_key.pub")"

KEYGEN_OUT="$("$LINK_BIN" keygen "$WORK/device_key")"
DEVICE_PUB="$(node -e 'const d = JSON.parse(process.argv[1]); console.log(d.public_key);' "$KEYGEN_OUT")"

printf 'command="%s gate --as phone",restrict %s\n' "$LEGION_BIN" "$DEVICE_PUB" > "$WORK/authorized_keys"
chmod 600 "$WORK/authorized_keys"

cat > "$WORK/sshd_config" <<EOF
ListenAddress 127.0.0.1
Port $PORT
HostKey $WORK/host_key
PidFile $WORK/sshd.pid
AuthorizedKeysFile $WORK/authorized_keys
AllowUsers $USER_NAME
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
UsePAM no
StrictModes no
SetEnv LEGION_HOME=$FAR_HOME
LogLevel VERBOSE
EOF

"$SSHD_BIN" -t -f "$WORK/sshd_config"
"$SSHD_BIN" -f "$WORK/sshd_config" -E "$WORK/sshd.log"

# Wait briefly for sshd to start listening on loopback.
node -e '
const net = require("node:net");
const port = Number(process.argv[1]);
const deadline = Date.now() + 5000;
function attempt() {
  const s = net.connect(port, "127.0.0.1");
  s.once("connect", () => { s.end(); process.exit(0); });
  s.once("error", () => {
    if (Date.now() > deadline) { console.error("sshd did not listen in time"); process.exit(1); }
    setTimeout(attempt, 50);
  });
}
attempt();
' "$PORT"

node -e '
const fs = require("node:fs");
const [configPath, keyPath, port, user, hostKey] = process.argv.slice(1);
const config = {
  device_key: fs.readFileSync(keyPath, "utf8"),
  nodes: [{
    node: "tower",
    target: {
      hosts: ["127.0.0.1"],
      port: Number(port),
      user,
      host_keys: [hostKey],
    },
    principal: "phone",
  }],
};
fs.writeFileSync(configPath, JSON.stringify(config, null, 2) + "\n", { mode: 0o600 });
' "$WORK/config.json" "$WORK/device_key" "$PORT" "$USER_NAME" "$HOST_PUB"

echo "1) Running legion-link sshd integration test..."
LEGION_LINK_SSHD="127.0.0.1:$PORT" \
LEGION_LINK_USER="$USER_NAME" \
LEGION_LINK_HOST_KEY="$HOST_PUB" \
LEGION_LINK_KEY_FILE="$WORK/device_key" \
LEGION_LINK_NODE="tower" \
  cargo test -p legion-link --features ssh --test sshd -- --ignored --nocapture

echo "2) Running example binary: link tools..."
TOOLS_OUT="$("$LINK_BIN" tools --config "$WORK/config.json")"
node -e '
const data = JSON.parse(process.argv[1]);
if (!Array.isArray(data.tools) || !data.tools.some(t => t.name === "sys.info")) {
  console.error("tools output missing sys.info:", data);
  process.exit(1);
}
console.log(`   listed ${data.tools.length} tools`);
' "$TOOLS_OUT"

echo "3) Running example binary: link call sys.info..."
SYS_OUT="$("$LINK_BIN" call --config "$WORK/config.json" sys.info '{}')"
node -e '
const data = JSON.parse(process.argv[1]);
if (!data.os || !data.os.family || !data.os.arch) {
  console.error("sys.info missing os.family/os.arch:", data);
  process.exit(1);
}
console.log(`   sys.info ok: os=${data.os.family} arch=${data.os.arch}`);
' "$SYS_OUT"

echo "4) Verifying gate enforces principal & policy..."
WHOAMI_OUT="$("$LINK_BIN" call --config "$WORK/config.json" meta.whoami '{}')"
node -e '
const data = JSON.parse(process.argv[1]);
if (data.principal !== "phone" || data.role !== "observer") {
  console.error("unexpected whoami:", data);
  process.exit(1);
}
console.log(`   meta.whoami ok: principal=${data.principal} role=${data.role}`);
' "$WHOAMI_OUT"

set +e
DENIED_OUT="$("$LINK_BIN" call --config "$WORK/config.json" exec.run '{"argv":["echo","nope"]}')"
DENIED_RC=$?
set -e
if [[ "$DENIED_RC" -ne 1 ]]; then
  echo "Expected exec.run to fail with exit 1, got $DENIED_RC: $DENIED_OUT" >&2
  exit 1
fi
node -e '
const data = JSON.parse(process.argv[1]);
if (data.code !== "denied") {
  console.error("expected denied error, got:", data);
  process.exit(1);
}
console.log(`   exec.run properly denied: ${data.message}`);
' "$DENIED_OUT"

echo "All legion-link sshd checks passed."
