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
sh /opt/retry-fetch.sh -- apt-get update -qq
sh /opt/retry-fetch.sh --quiet -- apt-get install -y -qq --no-install-recommends openssh-server openssh-client openssl python3

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
if /usr/sbin/sshd -G -f /dev/null -o PerSourcePenalties=no >/dev/null 2>&1; then
  echo 'PerSourcePenalties no' >> /etc/ssh/sshd_config_legion
fi

/usr/sbin/sshd -t -f /etc/ssh/sshd_config_legion
/usr/sbin/sshd -f /etc/ssh/sshd_config_legion -E /tmp/sshd-legion.log
trap 'cat /tmp/sshd-legion.log >&2 || true; cat /tmp/sshd-legion-2.log >&2 || true' ERR

# Second independent sshd (distinct host key and authorized_keys) for two-node fleet device tests.
ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key_box2
chmod 0644 /etc/ssh/ssh_host_ed25519_key_box2.pub
cat > /etc/ssh/sshd_config_legion_2 <<'SSHD_CFG2'
ListenAddress 127.0.0.1
Port 2223
HostKey /etc/ssh/ssh_host_ed25519_key_box2
PidFile /run/sshd-legion-2.pid
AuthorizedKeysFile .ssh/authorized_keys_box2
AllowUsers me
StrictModes yes
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
UsePAM no
Subsystem sftp internal-sftp
LogLevel VERBOSE
SSHD_CFG2
if /usr/sbin/sshd -G -f /dev/null -o PerSourcePenalties=no >/dev/null 2>&1; then
  echo 'PerSourcePenalties no' >> /etc/ssh/sshd_config_legion_2
fi

/usr/sbin/sshd -t -f /etc/ssh/sshd_config_legion_2
/usr/sbin/sshd -f /etc/ssh/sshd_config_legion_2 -E /tmp/sshd-legion-2.log

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
assert replies[2]["result"]["door"] == "ssh", replies[2]
assert replies[3]["result"]["content"] == "hello-from-container\n", replies[3]
assert replies[4]["error"]["data"]["code"] == "denied", replies[4]
PY
python3 - "$HOME/.local/state/legion/audit.jsonl" <<'PY'
import json, sys
with open(sys.argv[1], "r", encoding="utf-8") as fh:
    entries = [json.loads(line) for line in fh if line.strip()]
whoami = [e for e in entries if e["tool"] == "meta.whoami" and e.get("device") == "container-phone"]
assert whoami and whoami[-1]["door"] == "ssh", entries
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

# 7. device.list shows device; device.remove ends a held ControlMaster sshd connection and stops
#    both multiplexed reuse and fresh connections.
list_json="$(legion device.list --json)"
python3 -c 'import json,sys; d=json.loads(sys.argv[1]); assert len(d["invites"])==0 and len(d["devices"])==1 and d["devices"][0]["name"]=="container-phone", d' "$list_json"

ctl_active="/tmp/lgm-active-$$.sock"
ctl_fresh="/tmp/lgm-fresh-$$.sock"
ctl_repair_old="/tmp/lgm-repair-old-$$.sock"
cleanup_ctls() {
  ssh -S "$ctl_active" -O exit me@127.0.0.1 >/dev/null 2>&1 || true
  ssh -S "$ctl_fresh" -O exit me@127.0.0.1 >/dev/null 2>&1 || true
  ssh -S "$ctl_repair_old" -O exit me@127.0.0.1 >/dev/null 2>&1 || true
  rm -f "$ctl_active" "$ctl_fresh" "$ctl_repair_old"
}
trap cleanup_ctls EXIT

ssh -T -f -N \
  -o BatchMode=yes \
  -o IdentitiesOnly=yes \
  -o StrictHostKeyChecking=yes \
  -o "UserKnownHostsFile=$work/known_hosts" \
  -o GlobalKnownHostsFile=none \
  -o ConnectTimeout=10 \
  -o ControlMaster=yes \
  -o ControlPersist=yes \
  -S "$ctl_active" \
  -p 2222 \
  -i "$work/device_key" \
  me@127.0.0.1

mux_whoami="$(
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"call","params":{"tool":"meta.whoami","args":{}}}' \
  | ssh -T -o BatchMode=yes -o ControlMaster=no -S "$ctl_active" me@127.0.0.1 'legion rpc'
)"
python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["result"]["principal"]=="phone" and r["result"]["door"]=="ssh", r' "$mux_whoami"

rm_active_json="$(legion device.remove name=container-phone --json)"
python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["removed"] is True and r["sessions_ended"] >= 1, r' "$rm_active_json"

# Wait briefly for the ssh ControlMaster client to observe its terminated parent sshd.
mux_dead=0
for _ in $(seq 1 50); do
  if ! ssh -S "$ctl_active" -O check me@127.0.0.1 >/dev/null 2>&1; then
    mux_dead=1
    break
  fi
  sleep 0.1
done
[[ "$mux_dead" -eq 1 ]] || { echo "ControlMaster stayed alive after device.remove!" >&2; exit 1; }

if printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"call","params":{"tool":"meta.whoami","args":{}}}' \
  | ssh -T -o BatchMode=yes -o ControlMaster=no -S "$ctl_active" me@127.0.0.1 'legion rpc' >/dev/null 2>&1; then
  echo "revoked ControlMaster still answered legion rpc!" >&2
  exit 1
fi
if "${ssh_base[@]}" -i "$work/device_key" me@127.0.0.1 'legion rpc' </dev/null >/dev/null 2>&1; then
  echo "removed device key still authenticated!" >&2
  exit 1
fi
echo "    device.list, device.remove, and active ControlMaster teardown verified"

# 7b. A never-used `-fN` ControlMaster opened before device.remove is still refused when it tries
#     to open a session channel afterwards (`legion gate` re-checks authorized_keys after registering).
inv_fresh_json="$(legion device.invite principal=phone host=127.0.0.1 port=2222 --json)"
printf '%s\n' "$inv_fresh_json" > "$work/invite_fresh.json"
python3 - "$work/invite_fresh.json" "$work/temp_key_fresh" "$work/invite_fresh_id" <<'PY'
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
priv = b"\x11\x22\x33\x44" * 2 + ssh_str(b"ssh-ed25519") + ssh_str(pub32) + ssh_str(seed + pub32) + ssh_str(b"fresh")
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
with open(sys.argv[3], "w", encoding="utf-8") as fh:
    fh.write(payload["id"])
PY
ssh-keygen -q -t ed25519 -N '' -C 'fresh@test' -f "$work/fresh_key"
fresh_pub="$(cat "$work/fresh_key.pub")"
fresh_req="$(python3 -c 'import json,sys; print(json.dumps({"enrol":1,"id":sys.argv[1],"public_key":sys.argv[2],"name":"Container Fresh"}))' "$(cat "$work/invite_fresh_id")" "$fresh_pub")"
printf '%s\n' "$fresh_req" | "${ssh_base[@]}" -i "$work/temp_key_fresh" me@127.0.0.1 >/dev/null

ssh -T -f -N \
  -o BatchMode=yes \
  -o IdentitiesOnly=yes \
  -o StrictHostKeyChecking=yes \
  -o "UserKnownHostsFile=$work/known_hosts" \
  -o GlobalKnownHostsFile=none \
  -o ConnectTimeout=10 \
  -o ControlMaster=yes \
  -o ControlPersist=yes \
  -S "$ctl_fresh" \
  -p 2222 \
  -i "$work/fresh_key" \
  me@127.0.0.1

legion device.remove name=container-fresh --json >/dev/null

set +e
fresh_stderr="$(
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"call","params":{"tool":"meta.whoami","args":{}}}' \
  | ssh -T -o BatchMode=yes -o ControlMaster=no -S "$ctl_fresh" me@127.0.0.1 'legion rpc' 2>&1 >/dev/null
)"
fresh_rc=$?
set -e
[[ "$fresh_rc" -ne 0 ]] || { echo "never-used ControlMaster unexpectedly succeeded after device.remove!" >&2; exit 1; }
[[ "$fresh_stderr" == *"no longer authorized"* ]] || { echo "expected gate re-check refusal, got: $fresh_stderr" >&2; exit 1; }
fresh_dead=0
for _ in $(seq 1 50); do
  if ! ssh -S "$ctl_fresh" -O check me@127.0.0.1 >/dev/null 2>&1; then
    fresh_dead=1
    break
  fi
  sleep 0.1
done
[[ "$fresh_dead" -eq 1 ]] || { echo "never-used ControlMaster stayed alive after gate refusal!" >&2; exit 1; }
echo "    never-used ControlMaster gate re-check refusal and parent sshd teardown verified"

# 7c. Re-pairing under the exact same device name and principal MUST NOT revive an earlier
#     removed pairing's open ControlMaster (each pairing has its own random `--pairing <id>`).
enrol_named_device() {
  local dev_name="$1"
  local prefix="$2"
  local inv_json
  inv_json="$(legion device.invite principal=phone host=127.0.0.1 port=2222 --json)"
  printf '%s\n' "$inv_json" > "$work/${prefix}_inv.json"
  python3 - "$work/${prefix}_inv.json" "$work/${prefix}_temp_key" "$work/${prefix}_inv_id" <<'PY'
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
priv = b"\x11\x22\x33\x44" * 2 + ssh_str(b"ssh-ed25519") + ssh_str(pub32) + ssh_str(seed + pub32) + ssh_str(b"repair")
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
with open(sys.argv[3], "w", encoding="utf-8") as fh:
    fh.write(payload["id"])
PY
  ssh-keygen -q -t ed25519 -N '' -C "${prefix}@test" -f "$work/${prefix}_key"
  local dev_pub req
  dev_pub="$(cat "$work/${prefix}_key.pub")"
  req="$(python3 -c 'import json,sys; print(json.dumps({"enrol":1,"id":sys.argv[1],"public_key":sys.argv[2],"name":sys.argv[3]}))' "$(cat "$work/${prefix}_inv_id")" "$dev_pub" "$dev_name")"
  printf '%s\n' "$req" | "${ssh_base[@]}" -i "$work/${prefix}_temp_key" me@127.0.0.1 >/dev/null
}

enrol_named_device "container-repair" "repair_old"
ssh -T -f -N \
  -o BatchMode=yes \
  -o IdentitiesOnly=yes \
  -o StrictHostKeyChecking=yes \
  -o "UserKnownHostsFile=$work/known_hosts" \
  -o GlobalKnownHostsFile=none \
  -o ConnectTimeout=10 \
  -o ControlMaster=yes \
  -o ControlPersist=yes \
  -S "$ctl_repair_old" \
  -p 2222 \
  -i "$work/repair_old_key" \
  me@127.0.0.1

legion device.remove name=container-repair --json >/dev/null
enrol_named_device "container-repair" "repair_new"

set +e
repair_stderr="$(
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"call","params":{"tool":"meta.whoami","args":{}}}' \
  | ssh -T -o BatchMode=yes -o ControlMaster=no -S "$ctl_repair_old" me@127.0.0.1 'legion rpc' 2>&1 >/dev/null
)"
repair_rc=$?
set -e
[[ "$repair_rc" -ne 0 ]] || { echo "old ControlMaster was revived after re-pairing under the same name!" >&2; exit 1; }
[[ "$repair_stderr" == *"no longer authorized"* ]] || { echo "expected pairing-mismatch refusal, got: $repair_stderr" >&2; exit 1; }
repair_dead=0
for _ in $(seq 1 50); do
  if ! ssh -S "$ctl_repair_old" -O check me@127.0.0.1 >/dev/null 2>&1; then
    repair_dead=1
    break
  fi
  sleep 0.1
done
[[ "$repair_dead" -eq 1 ]] || { echo "old ControlMaster stayed alive after re-pair refusal!" >&2; exit 1; }

repair_new_out="$(
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"call","params":{"tool":"meta.whoami","args":{}}}' \
  | "${ssh_base[@]}" -i "$work/repair_new_key" me@127.0.0.1 'legion rpc'
)"
python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["result"]["principal"]=="phone", r' "$repair_new_out"
legion device.remove name=container-repair --json >/dev/null
echo "    same-name re-pair isolation verified"

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
python3 -c 'import json,sys; w=json.loads(sys.argv[1]); assert w["principal"]=="phone_link" and w["role"]=="operator" and w["door"]=="ssh", w' "$link_whoami_out"

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
python3 -c 'import json,sys; e=json.loads(sys.argv[1]); assert e["code"]=="denied" and e["details"]["reason"]=="device_removed_or_unknown" and e["details"].get("sent") is False, e' "$link_revoked_out"
echo "    legion-link revocation verified"

# 10. Fleet-wide paired devices across two nodes (box on 2222, box2 on 2223):
#     role-based pairing -> automatic fleet push -> device.endpoint --on all -> direct SSH to box2
#     -> device.set role upgrade -> device.remove with still_trusted_by -> fleet.sync revocation.
box2_home="$HOME/box2-legion"
mkdir -p "$box2_home/config" "$box2_home/state" "$HOME/.ssh"
chmod 700 "$HOME/.ssh"
ssh-keygen -q -t ed25519 -N '' -C 'fleet@container' -f "$work/fleet_key"
fleet_pub="$(cat "$work/fleet_key.pub")"
box2_hk="$(awk '{print $1 " " $2}' /etc/ssh/ssh_host_ed25519_key_box2.pub)"
box1_hk="$(awk '{print $1 " " $2}' /etc/ssh/ssh_host_ed25519_key.pub)"
printf '[127.0.0.1]:2222 %s\n' "$box1_hk" > "$work/fleet_kh_box1"
printf '[127.0.0.1]:2223 %s\n' "$box2_hk" > "$work/fleet_kh_box2"

# Put foreign fleet-link keys on box and box2 authorized_keys.
printf 'restrict,command="/usr/local/bin/legion rpc" %s fleet-link\n' "$fleet_pub" >> "$HOME/.ssh/authorized_keys"
printf 'restrict,command="env LEGION_HOME=%s /usr/local/bin/legion rpc" %s fleet-link\n' "$box2_home" "$fleet_pub" > "$HOME/.ssh/authorized_keys_box2"
chmod 600 "$HOME/.ssh/authorized_keys_box2"

cat > "$HOME/.config/legion/node.toml" <<NODE1
name = "box"

[devices]
hosts = ["127.0.0.1"]
port = 2222
user = "me"
host_keys = ["/etc/ssh/ssh_host_ed25519_key.pub"]
NODE1

cat > "$box2_home/config/node.toml" <<NODE2
name = "box2"

[devices]
authorized_keys = "$HOME/.ssh/authorized_keys_box2"
hosts = ["127.0.0.1"]
port = 2223
user = "me"
host_keys = ["/etc/ssh/ssh_host_ed25519_key_box2.pub"]
NODE2
cp "$HOME/.config/legion/policy.toml" "$box2_home/config/policy.toml"

cat > "$HOME/.config/legion/fleet.toml" <<FLEET1
[machines.box2]
command = ["ssh", "-T", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=yes", "-o", "UserKnownHostsFile=$work/fleet_kh_box2", "-o", "GlobalKnownHostsFile=none", "-o", "ControlMaster=no", "-o", "ConnectTimeout=3", "-o", "LogLevel=ERROR", "-p", "2223", "-i", "$work/fleet_key", "me@127.0.0.1", "legion rpc"]
FLEET1

cat > "$box2_home/config/fleet.toml" <<FLEET2
[machines.box]
command = ["ssh", "-T", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=yes", "-o", "UserKnownHostsFile=$work/fleet_kh_box1", "-o", "GlobalKnownHostsFile=none", "-o", "ControlMaster=no", "-o", "ConnectTimeout=3", "-o", "LogLevel=ERROR", "-p", "2222", "-i", "$work/fleet_key", "me@127.0.0.1", "legion rpc"]
FLEET2

fleet_inv_json="$(legion pair --name fleet-phone --role observer --json)"
printf '%s\n' "$fleet_inv_json" > "$work/fleet_inv.json"
python3 - "$work/fleet_inv.json" "$work/fleet_temp_key" "$work/fleet_inv_id" <<'PY'
import base64, json, os, struct, subprocess, sys
with open(sys.argv[1], "r", encoding="utf-8") as fh:
    res = json.load(fh)
assert res["principal"] == "device:fleet-phone" and res["role"] == "observer", res
invite = res["invite"]
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
priv = b"\x11\x22\x33\x44" * 2 + ssh_str(b"ssh-ed25519") + ssh_str(pub32) + ssh_str(seed + pub32) + ssh_str(b"fleet")
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
with open(sys.argv[3], "w", encoding="utf-8") as fh:
    fh.write(payload["id"])
PY

ssh-keygen -q -t ed25519 -N '' -C 'fleet-phone@test' -f "$work/fleet_phone_key"
fleet_phone_pub="$(cat "$work/fleet_phone_key.pub")"
fleet_req="$(python3 -c 'import json,sys; print(json.dumps({"enrol":1,"id":sys.argv[1],"public_key":sys.argv[2],"name":"ignored"}))' "$(cat "$work/fleet_inv_id")" "$fleet_phone_pub")"
fleet_enrol="$(printf '%s\n' "$fleet_req" | "${ssh_base[@]}" -i "$work/fleet_temp_key" me@127.0.0.1)"
python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["ok"] is True and r["principal"]=="device:fleet-phone" and r["role"]=="observer" and r["fleet"]["box2"]["status"]=="synced", r' "$fleet_enrol"

# Ask box for `device.endpoint` with `on: "all"` through fleet-phone, then connect directly to box2.
ep_out="$(
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"call","params":{"tool":"device.endpoint","args":{},"on":"all"}}' \
  | "${ssh_base[@]}" -i "$work/fleet_phone_key" me@127.0.0.1 'legion rpc'
)"
python3 - "$ep_out" "$work/learned_box2_kh" <<'PY'
import json, sys
r = json.loads(sys.argv[1])
res = r["result"]["results"]
assert res["box"]["ok"]["port"] == 2222, res
b2 = res["box2"]["ok"]
assert b2["node"] == "box2" and b2["port"] == 2223 and len(b2["host_keys"]) >= 1, b2
with open(sys.argv[2], "w", encoding="utf-8") as fh:
    for hk in b2["host_keys"]:
        fh.write(f"[127.0.0.1]:2223 {hk}\n")
PY

ssh_box2=(
  ssh -T
  -o BatchMode=yes
  -o IdentitiesOnly=yes
  -o StrictHostKeyChecking=yes
  -o "UserKnownHostsFile=$work/learned_box2_kh"
  -o GlobalKnownHostsFile=none
  -o ControlMaster=no
  -o ConnectTimeout=10
  -p 2223
)
box2_whoami="$(
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"call","params":{"tool":"meta.whoami","args":{}}}' \
  | "${ssh_box2[@]}" -i "$work/fleet_phone_key" me@127.0.0.1 'legion rpc'
)"
python3 -c 'import json,sys; r=json.loads(sys.argv[1])["result"]; assert r["node"]=="box2" and r["principal"]=="device:fleet-phone" and r["role"]=="observer" and r["door"]=="ssh", r' "$box2_whoami"

# Upgrade role via `device.set` on box and confirm it propagates to box2.
set_out="$(legion device.set name=fleet-phone role=operator --json)"
python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["role"]=="operator" and r["previous_role"]=="observer" and r["fleet"]["box2"]["status"]=="synced", r' "$set_out"
box2_exec="$(
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"call","params":{"tool":"exec.run","args":{"cmd":"printf upgraded"}}}' \
  | "${ssh_box2[@]}" -i "$work/fleet_phone_key" me@127.0.0.1 'legion rpc'
)"
python3 -c 'import json,sys; msgs=[json.loads(l) for l in sys.argv[1].splitlines() if l.strip()]; r=[m["result"] for m in msgs if m.get("id")==1 and "result" in m][0]; assert r["exit_code"]==0 and r["stdout"]=="upgraded", msgs' "$box2_exec"

# Remove on box2 while box is unreachable -> still_trusted_by names box -> sync revokes on box.
sed -i 's/"2222"/"2299"/' "$box2_home/config/fleet.toml"
rm_box2="$(LEGION_HOME="$box2_home" legion device.remove name=fleet-phone --json)"
python3 -c 'import json,sys; r=json.loads(sys.argv[1]); assert r["removed"] is True and r["still_trusted_by"]==["box"], r' "$rm_box2"

sed -i 's/"2299"/"2222"/' "$box2_home/config/fleet.toml"
LEGION_HOME="$box2_home" legion fleet.sync direction=push documents='["devices.toml"]' --json >/dev/null
if "${ssh_base[@]}" -i "$work/fleet_phone_key" me@127.0.0.1 'legion rpc' </dev/null >/dev/null 2>&1; then
  echo "fleet-phone still authenticated on box after fleet.sync revocation!" >&2
  exit 1
fi

# Trust rule check: planting an extra live owner entry on box2 must NOT be imported by box on push.
ssh-keygen -q -t ed25519 -N '' -C 'planted@box2' -f "$work/planted_key"
planted_pub="$(awk '{print $1 " " $2}' "$work/planted_key.pub")"
python3 - "$box2_home/config/devices.toml" "$planted_pub" <<'PY'
import datetime, sys
path, pub = sys.argv[1], sys.argv[2]
now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
with open(path, "a", encoding="utf-8") as fh:
    fh.write(
        '\n[devices.dddddddddddddddddddddddddddddddd]\n'
        'name = "planted-owner"\n'
        f'public_key = "{pub}"\n'
        'principal = "owner"\n'
        'role = "owner"\n'
        f'paired_at = "{now}"\n'
        'paired_on = "box2"\n'
        f'updated_at = "{now}"\n'
        'updated_on = "box2"\n'
    )
PY
legion fleet.sync direction=push documents='["devices.toml"]' --json >/dev/null
if grep -q 'dddddddddddddddddddddddddddddddd' "$HOME/.config/legion/devices.toml"; then
  echo "box imported live planted-owner entry from box2 during push!" >&2
  exit 1
fi
if "${ssh_base[@]}" -i "$work/planted_key" me@127.0.0.1 'legion rpc' </dev/null >/dev/null 2>&1; then
  echo "planted-owner key authenticated on box after push!" >&2
  exit 1
fi
echo "    two-node fleet device pairing, endpoint discovery, device.set, offline revocation sync, and push trust rule verified"
USER_EOF

chmod 0755 /tmp/pairing-user-test.sh
su - me -s /bin/bash -c 'bash /tmp/pairing-user-test.sh' </dev/null

echo "==> OpenSSH pairing verification passed"
CONTAINER_EOF
)"

"${DOCKER_CMD[@]}" run --rm \
  -v "$ROOT/scripts/retry-fetch.sh:/opt/retry-fetch.sh:ro" \
  -v "$BIN:/usr/local/bin/legion:ro" \
  -v "$LINK_BIN:/usr/local/bin/legion-link-cli:ro" \
  "$IMAGE" \
  bash -euo pipefail -c "$CONTAINER_SCRIPT"
