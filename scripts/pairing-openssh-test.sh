#!/usr/bin/env bash
# Verify the full SSH device-pairing flow against a real sshd inside a clean Linux container
# (for example ubuntu:22.04 with OpenSSH 8.9 or debian:12 with OpenSSH 9.2), exercising both the
# system OpenSSH client and `legion-link`'s in-process SSH client (`link` example binary).
#
# Usage:
#   bash scripts/pairing-openssh-test.sh <docker-image> [--bin <static-legion>] [--link-bin <static-link>]
#
# Defaults to:
#   target/x86_64-unknown-linux-musl/release/legion
#   target/x86_64-unknown-linux-musl/release/examples/link
#
# Build both first with:
#   cargo build --release --locked --target x86_64-unknown-linux-musl -p legion-cli
#   CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --locked --target x86_64-unknown-linux-musl -p legion-link --features ssh --example link
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
IMAGE=""
BIN="$ROOT/target/x86_64-unknown-linux-musl/release/legion"
LINK_BIN="$ROOT/target/x86_64-unknown-linux-musl/release/examples/link"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin)
      [[ $# -ge 2 ]] || { echo "pairing-openssh-test: --bin needs a path" >&2; exit 2; }
      BIN="$2"
      shift 2
      ;;
    --bin=*)
      BIN="${1#--bin=}"
      shift
      ;;
    --link-bin)
      [[ $# -ge 2 ]] || { echo "pairing-openssh-test: --link-bin needs a path" >&2; exit 2; }
      LINK_BIN="$2"
      shift 2
      ;;
    --link-bin=*)
      LINK_BIN="${1#--link-bin=}"
      shift
      ;;
    -h|--help)
      echo "Usage: bash scripts/pairing-openssh-test.sh <docker-image> [--bin <static-legion>] [--link-bin <static-link>]"
      exit 0
      ;;
    -*)
      echo "pairing-openssh-test: unknown option $1" >&2
      exit 2
      ;;
    *)
      if [[ -z "$IMAGE" ]]; then
        IMAGE="$1"
      else
        echo "pairing-openssh-test: unexpected argument $1" >&2
        exit 2
      fi
      shift
      ;;
  esac
done

if [[ -z "$IMAGE" ]]; then
  echo "Usage: bash scripts/pairing-openssh-test.sh <docker-image> [--bin <static-legion>] [--link-bin <static-link>]" >&2
  exit 2
fi

if [[ ! -x "$BIN" ]]; then
  echo "pairing-openssh-test: legion executable not found at $BIN" >&2
  echo "Build it with: cargo build --release --locked --target x86_64-unknown-linux-musl -p legion-cli" >&2
  exit 1
fi
BIN="$(cd -- "$(dirname -- "$BIN")" && pwd -P)/$(basename -- "$BIN")"

if [[ ! -x "$LINK_BIN" ]]; then
  echo "pairing-openssh-test: legion-link example executable not found at $LINK_BIN" >&2
  echo "Build it with: CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --locked --target x86_64-unknown-linux-musl -p legion-link --features ssh --example link" >&2
  exit 1
fi
LINK_BIN="$(cd -- "$(dirname -- "$LINK_BIN")" && pwd -P)/$(basename -- "$LINK_BIN")"

if ! command -v docker >/dev/null 2>&1; then
  echo "pairing-openssh-test: docker is required" >&2
  exit 1
fi

DOCKER_CMD=(docker)
if ! docker info >/dev/null 2>&1; then
  if sudo -n docker info >/dev/null 2>&1; then
    DOCKER_CMD=(sudo docker)
  else
    echo "pairing-openssh-test: cannot reach Docker daemon (neither 'docker' nor 'sudo -n docker' succeeded)" >&2
    exit 1
  fi
fi

echo "==> Running OpenSSH pairing verification in $IMAGE"
echo "    legion: $BIN"
echo "    link:   $LINK_BIN"

CONTAINER_SCRIPT="$(cat <<'CONTAINER_EOF'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends openssh-server openssh-client openssl python3 >/dev/null

echo "==> OpenSSH version:"
sshd -V 2>&1 || true
ssh -V 2>&1 || true

# Create a non-root account unlocked for public-key authentication.
useradd -m -s /bin/bash me
usermod -p '*' me

mkdir -p /run/sshd
ssh-keygen -A >/dev/null

cat > /etc/ssh/sshd_config_legion <<'SSHD_CFG'
ListenAddress 127.0.0.1
Port 2222
HostKey /etc/ssh/ssh_host_ed25519_key
PidFile /run/sshd-legion.pid
AuthorizedKeysFile .ssh/authorized_keys
AllowUsers me
StrictModes yes
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
UsePAM no
Subsystem sftp internal-sftp
LogLevel VERBOSE
SSHD_CFG

/usr/sbin/sshd -t -f /etc/ssh/sshd_config_legion
/usr/sbin/sshd -f /etc/ssh/sshd_config_legion -E /tmp/sshd-legion.log
trap 'cat /tmp/sshd-legion.log >&2 || true' ERR

cat > /tmp/pairing-user-test.sh <<'USER_EOF'
set -euo pipefail

legion --version
legion init --name box --json >/dev/null

cat > "$HOME/.config/legion/policy.toml" <<'POLICY'
default = "owner"

[roles.observer]
allow = ["effect:read"]
deny = ["audit.*"]
paths = ["~/projects/**"]

[roles.operator]
allow = ["effect:read", "exec.run"]
deny = ["audit.*", "fs.write"]

[principals.phone]
role = "observer"

[principals.phone_link]
role = "operator"
POLICY
mkdir -p "$HOME/projects"
printf 'hello-from-container\n' > "$HOME/projects/note.txt"

work="$HOME/pair-work"
mkdir -p "$work"
chmod 700 "$work"

# 1. Create a pairing invite targeting this container loopback sshd.
invite_json="$(legion device.invite principal=phone host=127.0.0.1 port=2222 --json)"
printf '%s\n' "$invite_json" > "$work/invite.json"

# Verify ~/.ssh and authorized_keys modes and 12-digit expiry-time without Z.
ssh_mode="$(stat -c '%a' "$HOME/.ssh")"
ak_mode="$(stat -c '%a' "$HOME/.ssh/authorized_keys")"
[[ "$ssh_mode" == "700" ]] || { echo "expected ~/.ssh mode 700, got $ssh_mode" >&2; exit 1; }
[[ "$ak_mode" == "600" ]] || { echo "expected authorized_keys mode 600, got $ak_mode" >&2; exit 1; }
grep -Eq 'expiry-time="[0-9]{12}"' "$HOME/.ssh/authorized_keys" || {
  echo "expected 12-digit expiry-time without Z in authorized_keys:" >&2
  cat "$HOME/.ssh/authorized_keys" >&2
  exit 1
}

# 2. Decode the invite, materialize the temp OpenSSH private key and pinned known_hosts.
python3 - "$work/invite.json" "$work/temp_key" "$work/known_hosts" "$work/invite_id" <<'PY'
import base64, json, os, struct, subprocess, sys

invite_path, key_out, kh_out, id_out = sys.argv[1:5]
with open(invite_path, "r", encoding="utf-8") as fh:
    res = json.load(fh)

assert res["host_keys_source"].startswith("ssh-keyscan"), res
invite = res["invite"]
assert invite.startswith("legion-invite-1."), "bad invite prefix"
raw = invite[len("legion-invite-1."):]
raw += "=" * ((4 - len(raw) % 4) % 4)
payload = json.loads(base64.urlsafe_b64decode(raw).decode("utf-8"))
assert payload["v"] == 1
assert payload["node"] == "box"
assert payload["principal"] == "phone"
assert payload["port"] == 2222
assert payload["user"] == "me"
assert len(payload["host_keys"]) >= 1

seed = base64.b64decode(payload["key"])
assert len(seed) == 32

pkcs8 = bytes.fromhex("302e020100300506032b657004220420") + seed
proc = subprocess.run(
    ["openssl", "pkey", "-inform", "DER", "-pubout", "-outform", "DER"],
    input=pkcs8,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=True,
)
pub32 = proc.stdout[-32:]
assert len(pub32) == 32

def ssh_str(data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + data

pub_blob = ssh_str(b"ssh-ed25519") + ssh_str(pub32)
check = b"\x11\x22\x33\x44"
priv = (
    check
    + check
    + ssh_str(b"ssh-ed25519")
    + ssh_str(pub32)
    + ssh_str(seed + pub32)
    + ssh_str(b"legion-test")
)
pad = 1
while len(priv) % 8 != 0:
    priv += bytes([pad])
    pad += 1

blob = (
    b"openssh-key-v1\x00"
    + ssh_str(b"none")
    + ssh_str(b"none")
    + ssh_str(b"")
    + struct.pack(">I", 1)
    + ssh_str(pub_blob)
    + ssh_str(priv)
)
b64 = base64.b64encode(blob).decode("ascii")
lines = [b64[i:i + 70] for i in range(0, len(b64), 70)]
pem = "-----BEGIN OPENSSH PRIVATE KEY-----\n" + "\n".join(lines) + "\n-----END OPENSSH PRIVATE KEY-----\n"

fd = os.open(key_out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w", encoding="ascii") as fh:
    fh.write(pem)

with open(kh_out, "w", encoding="utf-8") as fh:
    for hk in payload["host_keys"]:
        fh.write(f"[127.0.0.1]:2222 {hk}\n")

with open(id_out, "w", encoding="utf-8") as fh:
    fh.write(payload["id"])
PY

invite_id="$(cat "$work/invite_id")"
ssh-keygen -q -t ed25519 -N '' -C 'phone@test' -f "$work/device_key"
device_pub="$(cat "$work/device_key.pub")"

ssh_base=(
  ssh -T
  -o BatchMode=yes
  -o IdentitiesOnly=yes
  -o StrictHostKeyChecking=yes
  -o "UserKnownHostsFile=$work/known_hosts"
  -o GlobalKnownHostsFile=none
  -o ControlMaster=no
  -o ConnectTimeout=10
  -p 2222
)

# 3. Enrol over real ssh (with arbitrary remote command ignored by --enrol).
enrol_req="$(python3 -c 'import json,sys; print(json.dumps({"enrol":1,"id":sys.argv[1],"public_key":sys.argv[2],"name":"Container Phone"}))' "$invite_id" "$device_pub")"
enrol_reply="$(printf '%s\n' "$enrol_req" | "${ssh_base[@]}" -i "$work/temp_key" me@127.0.0.1 id)"
python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r == {"ok":True,"node":"box","principal":"phone","role":"observer","device":"container-phone"}, r' "$enrol_reply"
echo "    enrolled (system ssh): $enrol_reply"

# 4. Run a real legion rpc session with the enrolled device key.
rpc_out="$(
  printf '%s\n%s\n%s\n%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"hello","params":{"client":"openssh-test","version":"1","protocol":1}}' \
    '{"jsonrpc":"2.0","id":2,"method":"call","params":{"tool":"meta.whoami","args":{},"as":"owner"}}' \
    '{"jsonrpc":"2.0","id":3,"method":"call","params":{"tool":"fs.read","args":{"path":"~/projects/note.txt"}}}' \
    '{"jsonrpc":"2.0","id":4,"method":"call","params":{"tool":"fs.write","args":{"path":"~/projects/nope.txt","content":"x"}}}' \
  | "${ssh_base[@]}" -i "$work/device_key" me@127.0.0.1 'legion rpc'
)"
python3 - "$rpc_out" <<'PY'
import json, sys
replies = {}
for line in sys.argv[1].splitlines():
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    if "id" in msg and "method" not in msg:
        replies[msg["id"]] = msg
assert replies[1]["result"]["node"] == "box", replies[1]
assert replies[2]["result"]["principal"] == "phone", replies[2]
assert replies[2]["result"]["role"] == "observer", replies[2]
assert replies[3]["result"]["content"] == "hello-from-container\n", replies[3]
assert replies[4]["error"]["data"]["code"] == "denied", replies[4]
PY
echo "    legion rpc session verified (system ssh)"

# 5. Arbitrary commands get legion gate refusal.
set +e
gate_stderr="$("${ssh_base[@]}" -i "$work/device_key" me@127.0.0.1 'whoami' </dev/null 2>&1 >/dev/null)"
gate_rc=$?
set -e
[[ "$gate_rc" -ne 0 ]] || { echo "arbitrary command unexpectedly succeeded" >&2; exit 1; }
[[ "$gate_stderr" == *"legion gate:"* ]] || { echo "expected legion gate refusal, got: $gate_stderr" >&2; exit 1; }
echo "    gate refusal verified"

# 6. Temporary invite key no longer authenticates.
if "${ssh_base[@]}" -i "$work/temp_key" me@127.0.0.1 'legion rpc' </dev/null >/dev/null 2>&1; then
  echo "temporary invite key still authenticated!" >&2
  exit 1
fi
echo "    temp key revocation verified"

# 7. device.list shows device and device.remove stops the key working.
list_json="$(legion device.list --json)"
python3 -c 'import json,sys; d=json.loads(sys.argv[1]); assert len(d["invites"])==0 and len(d["devices"])==1 and d["devices"][0]["name"]=="container-phone", d' "$list_json"
legion device.remove name=container-phone --json >/dev/null
if "${ssh_base[@]}" -i "$work/device_key" me@127.0.0.1 'legion rpc' </dev/null >/dev/null 2>&1; then
  echo "removed device key still authenticated!" >&2
  exit 1
fi
echo "    device.list and device.remove verified"

# 8. sshd itself enforces a past 12-digit expiry-time without Z.
inv2_json="$(legion device.invite principal=phone host=127.0.0.1 port=2222 --json)"
printf '%s\n' "$inv2_json" > "$work/invite2.json"
python3 - "$work/invite2.json" "$work/temp_key2" <<'PY'
import base64, json, os, struct, subprocess, sys
with open(sys.argv[1], "r", encoding="utf-8") as fh:
    invite = json.load(fh)["invite"]
raw = invite[len("legion-invite-1."):]
raw += "=" * ((4 - len(raw) % 4) % 4)
payload = json.loads(base64.urlsafe_b64decode(raw).decode("utf-8"))
seed = base64.b64decode(payload["key"])
pkcs8 = bytes.fromhex("302e020100300506032b657004220420") + seed
pub32 = subprocess.run(
    ["openssl", "pkey", "-inform", "DER", "-pubout", "-outform", "DER"],
    input=pkcs8, stdout=subprocess.PIPE, check=True,
).stdout[-32:]
def ssh_str(data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + data
pub_blob = ssh_str(b"ssh-ed25519") + ssh_str(pub32)
priv = b"\x11\x22\x33\x44" * 2 + ssh_str(b"ssh-ed25519") + ssh_str(pub32) + ssh_str(seed + pub32) + ssh_str(b"exp")
pad = 1
while len(priv) % 8 != 0:
    priv += bytes([pad])
    pad += 1
blob = b"openssh-key-v1\x00" + ssh_str(b"none") + ssh_str(b"none") + ssh_str(b"") + struct.pack(">I", 1) + ssh_str(pub_blob) + ssh_str(priv)
b64 = base64.b64encode(blob).decode("ascii")
lines = [b64[i:i + 70] for i in range(0, len(b64), 70)]
pem = "-----BEGIN OPENSSH PRIVATE KEY-----\n" + "\n".join(lines) + "\n-----END OPENSSH PRIVATE KEY-----\n"
fd = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w", encoding="ascii") as fh:
    fh.write(pem)
PY
sed -i -E 's/expiry-time="[0-9]{12}"/expiry-time="202001010000"/' "$HOME/.ssh/authorized_keys"
if "${ssh_base[@]}" -i "$work/temp_key2" me@127.0.0.1 </dev/null >/dev/null 2>&1; then
  echo "sshd accepted an expired 12-digit expiry-time key!" >&2
  exit 1
fi
echo "    sshd expiry-time enforcement verified"

# 9. End-to-end pairing and calls with the in-process legion-link client (`russh`).
legion-link-cli keygen "$work/link_device_key" >/dev/null
link_inv_json="$(legion device.invite principal=phone_link host=127.0.0.1 port=2222 --json)"
python3 - "$link_inv_json" "$work/link_invite.txt" <<'PY'
import json, os, sys
res = json.loads(sys.argv[1])
assert res["host_keys_source"].startswith("ssh-keyscan"), res
fd = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w", encoding="utf-8") as fh:
    fh.write(res["invite"] + "\n")
PY

link_enrol_out="$(legion-link-cli enrol \
  --invite-file "$work/link_invite.txt" \
  --key "$work/link_device_key" \
  --name "Container Link Phone" \
  --out "$work/link_config.json")"
python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["node"]=="box" and r["principal"]=="phone_link" and r["role"]=="operator" and r["device"]=="container-link-phone", r' "$link_enrol_out"
echo "    enrolled (legion-link): $link_enrol_out"

link_tools_out="$(legion-link-cli tools --config "$work/link_config.json")"
python3 -c 'import json,sys; names=[t["name"] for t in json.loads(sys.argv[1])["tools"]]; assert "meta.whoami" in names and "exec.run" in names, names' "$link_tools_out"

link_whoami_out="$(legion-link-cli call --config "$work/link_config.json" meta.whoami '{}')"
python3 -c 'import json,sys; w=json.loads(sys.argv[1]); assert w["principal"]=="phone_link" and w["role"]=="operator" and w["door"]=="rpc", w' "$link_whoami_out"

link_exec_out="$(legion-link-cli call --config "$work/link_config.json" exec.run '{"cmd":"printf '\''from-legion-link\\n'\''"}' 2>"$work/link_exec.stderr")"
python3 -c 'import json,sys; e=json.loads(sys.argv[1]); assert e["exit_code"]==0 and e["stdout"]=="from-legion-link\n", e' "$link_exec_out"
grep -q 'from-legion-link' "$work/link_exec.stderr" || {
  echo "expected streamed stdout event on stderr from legion-link-cli, got:" >&2
  cat "$work/link_exec.stderr" >&2
  exit 1
}

set +e
link_denied_out="$(legion-link-cli call --config "$work/link_config.json" fs.write '{"path":"~/projects/blocked.txt","content":"x"}')"
link_denied_rc=$?
set -e
[[ "$link_denied_rc" -eq 1 ]] || { echo "expected fs.write to fail with exit 1, got $link_denied_rc: $link_denied_out" >&2; exit 1; }
python3 -c 'import json,sys; e=json.loads(sys.argv[1]); assert e["code"]=="denied", e' "$link_denied_out"
echo "    legion-link tools, meta.whoami, streamed exec.run, and policy denial verified"

legion device.remove name=container-link-phone --json >/dev/null
set +e
link_revoked_out="$(legion-link-cli call --config "$work/link_config.json" meta.whoami '{}')"
link_revoked_rc=$?
set -e
[[ "$link_revoked_rc" -eq 1 ]] || { echo "expected revoked legion-link call to exit 1, got $link_revoked_rc: $link_revoked_out" >&2; exit 1; }
python3 -c 'import json,sys; e=json.loads(sys.argv[1]); assert e["code"]=="denied" and e["details"]["reason"]=="auth_failed", e' "$link_revoked_out"
echo "    legion-link revocation verified"
USER_EOF

chmod 0755 /tmp/pairing-user-test.sh
su - me -s /bin/bash -c 'bash /tmp/pairing-user-test.sh' </dev/null

echo "==> OpenSSH pairing verification passed"
CONTAINER_EOF
)"

"${DOCKER_CMD[@]}" run --rm \
  -v "$BIN:/usr/local/bin/legion:ro" \
  -v "$LINK_BIN:/usr/local/bin/legion-link-cli:ro" \
  "$IMAGE" \
  bash -euo pipefail -c "$CONTAINER_SCRIPT"
