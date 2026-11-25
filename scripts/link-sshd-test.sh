#!/usr/bin/env bash
# End-to-end test of `legion-link`'s in-process SSH client (`russh`) against throwaway loopback
# `sshd` instances forcing `legion gate --as phone` with a throwaway `LEGION_HOME`.
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
  for pidfile in "$WORK/sshd.pid" "$WORK/sshd_mismatch.pid"; do
    if [[ -f "$pidfile" ]]; then
      local pid
      pid="$(cat "$pidfile" 2>/dev/null || true)"
      if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
      fi
    fi
  done
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

cat > "$FAR_HOME/config/policy.toml" <<EOF
default = "owner"

[principals.phone]
role = "observer"

[principals.writer]
role = "editor"

[roles.observer]
allow = ["effect:read"]

[roles.editor]
allow = ["effect:read", "effect:write"]
paths = ["$WORK/uploads", "$WORK/sample-content.bin"]
EOF

mkdir -p "$WORK/uploads"

# Primary server holds both ed25519 and ecdsa host keys to exercise pin ordering & retry-once.
ssh-keygen -q -t ed25519 -N '' -C "legion-link-host-ed25519" -f "$WORK/host_ed25519"
ssh-keygen -q -t ecdsa -b 256 -N '' -C "legion-link-host-ecdsa" -f "$WORK/host_ecdsa"
# Secondary server on 127.0.0.3:$PORT holds a distinct unpinned host key to test host-list fallback.
ssh-keygen -q -t ed25519 -N '' -C "legion-link-host-other" -f "$WORK/host_other"
chmod 600 "$WORK/host_ed25519" "$WORK/host_ecdsa" "$WORK/host_other"
HOST_PUB_ED25519="$(cat "$WORK/host_ed25519.pub")"
HOST_PUB_ECDSA="$(cat "$WORK/host_ecdsa.pub")"

KEYGEN_OUT="$("$LINK_BIN" keygen "$WORK/device_key")"
DEVICE_PUB="$(node -e 'const d = JSON.parse(process.argv[1]); console.log(d.public_key);' "$KEYGEN_OUT")"
UPLOAD_KEYGEN_OUT="$("$LINK_BIN" keygen "$WORK/upload_key")"
UPLOAD_PUB="$(node -e 'const d = JSON.parse(process.argv[1]); console.log(d.public_key);' "$UPLOAD_KEYGEN_OUT")"

{
  printf 'command="%s gate --as phone --home %s",restrict %s\n' "$LEGION_BIN" "$FAR_HOME" "$DEVICE_PUB"
  printf 'command="%s gate --as writer --home %s",restrict %s\n' "$LEGION_BIN" "$FAR_HOME" "$UPLOAD_PUB"
} > "$WORK/authorized_keys"
chmod 600 "$WORK/authorized_keys"

cat > "$WORK/sshd_config" <<EOF
ListenAddress 127.0.0.1
Port $PORT
HostKey $WORK/host_ed25519
HostKey $WORK/host_ecdsa
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

cat > "$WORK/sshd_mismatch_config" <<EOF
ListenAddress 127.0.0.3
Port $PORT
HostKey $WORK/host_other
PidFile $WORK/sshd_mismatch.pid
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

MISMATCH_HOST=""
if "$SSHD_BIN" -t -f "$WORK/sshd_mismatch_config" 2>/dev/null \
  && "$SSHD_BIN" -f "$WORK/sshd_mismatch_config" -E "$WORK/sshd_mismatch.log" 2>/dev/null; then
  MISMATCH_HOST="127.0.0.3"
fi

wait_listen() {
  local host="$1"
  node -e '
const net = require("node:net");
const [host, portStr] = process.argv.slice(1);
const port = Number(portStr);
const deadline = Date.now() + 5000;
function attempt() {
  const s = net.connect(port, host);
  s.once("connect", () => { s.end(); process.exit(0); });
  s.once("error", () => {
    if (Date.now() > deadline) { console.error(`sshd on ${host}:${port} did not listen in time`); process.exit(1); }
    setTimeout(attempt, 50);
  });
}
attempt();
' "$host" "$PORT"
}

wait_listen "127.0.0.1"
if [[ -n "$MISMATCH_HOST" ]]; then
  wait_listen "$MISMATCH_HOST"
fi

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
' "$WORK/config.json" "$WORK/device_key" "$PORT" "$USER_NAME" "$HOST_PUB_ED25519"

NODE_BIN="$(command -v node)"
cat > "$WORK/enrol-responder.sh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec "$NODE_BIN" -e '
const fs = require("node:fs");
const [authKeysPath, legionBin, farHome] = process.argv.slice(1);
let buf = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buf += chunk;
  const nl = buf.indexOf("\n");
  if (nl === -1) return;
  const line = buf.slice(0, nl).trim();
  const req = JSON.parse(line);
  if (req.enrol !== 1 || typeof req.public_key !== "string" || !req.public_key.startsWith("ssh-ed25519 ") || /[\r\n]/.test(req.public_key)) {
    process.stdout.write(JSON.stringify({ ok: false, error: { code: "invalid_args", message: "Bad enrolment request." } }) + "\n");
    process.exit(0);
  }
  const entry = \`command="\${legionBin} gate --as phone --home \${farHome}",restrict \${req.public_key}\n\`;
  fs.appendFileSync(authKeysPath, entry, { mode: 0o600 });
  process.stdout.write(JSON.stringify({
    ok: true,
    node: "tower",
    principal: "phone",
    role: "observer",
    device: String(req.name || "device"),
  }) + "\n");
  process.exit(0);
});
' "$WORK/authorized_keys" "$LEGION_BIN" "$FAR_HOME"
EOF
chmod 700 "$WORK/enrol-responder.sh"

echo "1) Running legion-link sshd integration test..."
SAMPLE_FILE="$WORK/sample-content.bin"
node -e '
const fs = require("node:fs");
// 5 MiB (> russh 2 MiB window and 100-packet mpsc) so unread-stream non-blocking is proven.
const buf = Buffer.alloc(5 * 1024 * 1024);
for (let i = 0; i < buf.length; i++) buf[i] = (i * 31 + 7) & 0xff;
fs.writeFileSync(process.argv[1], buf);
' "$SAMPLE_FILE"

LEGION_HOME="$FAR_HOME" \
LEGION_LINK_SSHD="127.0.0.1:$PORT" \
LEGION_LINK_USER="$USER_NAME" \
LEGION_LINK_HOST_KEY="$HOST_PUB_ED25519" \
LEGION_LINK_HOST_KEY_ECDSA="$HOST_PUB_ECDSA" \
LEGION_LINK_MISMATCH_HOST="$MISMATCH_HOST" \
LEGION_LINK_KEY_FILE="$WORK/device_key" \
LEGION_LINK_UPLOAD_KEY_FILE="$WORK/upload_key" \
LEGION_LINK_UPLOAD_DIR="$WORK/uploads" \
LEGION_LINK_AUTH_KEYS="$WORK/authorized_keys" \
LEGION_LINK_ENROL_SCRIPT="$WORK/enrol-responder.sh" \
LEGION_LINK_SAMPLE_FILE="$SAMPLE_FILE" \
LEGION_LINK_BIN="$LEGION_BIN" \
LEGION_LINK_NODE="tower" \
  cargo test -p legion-link --features ssh,local --test sshd -- --ignored --nocapture

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

echo "5) Verifying link enrol refuses positional invite argument..."
set +e
ARGV_OUT="$("$LINK_BIN" enrol "legion-invite-1.secret" --key "$WORK/device_key" --name phone --out "$WORK/out.json")"
ARGV_RC=$?
set -e
if [[ "$ARGV_RC" -ne 2 ]]; then
  echo "Expected positional invite arg to exit 2, got $ARGV_RC: $ARGV_OUT" >&2
  exit 1
fi
echo "   enrol positional argument properly refused (exit 2)"

echo "All legion-link sshd checks passed."
