#!/usr/bin/env bash
# Deploy horus-msg-relay to the production VPS (builds on the server — same flow as wake-relay).
# Usage: ./deploy.sh [user@host]
# Env: SSH_KEY (default ~/.ssh/horus-oci)
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../../.." && pwd)"
RELAY_DIR="$REPO/infrastructure/relays/dev-relay"
HOST="${1:-root@wake.a10x.eu}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/horus-oci}"
SSH_OPTS=(-i "$SSH_KEY" -o BatchMode=yes -o StrictHostKeyChecking=accept-new)
REMOTE_DIR="/tmp/horus-msg-relay-build"

echo "==> Syncing dev-relay + horus-registry to $HOST..."
ssh "${SSH_OPTS[@]}" "$HOST" "rm -rf $REMOTE_DIR && mkdir -p $REMOTE_DIR/infrastructure/relays $REMOTE_DIR/packages"
rsync -az --delete -e "ssh ${SSH_OPTS[*]}" \
  --exclude target \
  "$RELAY_DIR/" "$HOST:$REMOTE_DIR/infrastructure/relays/dev-relay/"
# horus-registry is Rust-only; skip Motoko canister + local dfx/icp caches (broken overlays break rsync).
rsync -az --delete -e "ssh ${SSH_OPTS[*]}" \
  --exclude target \
  --exclude canister \
  --exclude .icp \
  --exclude .dfx \
  --exclude .mops \
  "$REPO/packages/blockchain/" "$HOST:$REMOTE_DIR/packages/blockchain/"

echo "==> Building on remote host..."
ssh "${SSH_OPTS[@]}" "$HOST" "source \"\$HOME/.cargo/env\" 2>/dev/null || true; cd $REMOTE_DIR/infrastructure/relays/dev-relay && cargo build --release"

echo "==> Installing binary..."
ssh "${SSH_OPTS[@]}" "$HOST" "install -m 755 $REMOTE_DIR/infrastructure/relays/dev-relay/target/release/horus-dev-relay /usr/local/bin/horus-msg-relay"

echo "==> Installing systemd service..."
scp "${SSH_OPTS[@]}" "$(dirname "$0")/horus-msg-relay.service" "$HOST:/tmp/horus-msg-relay.service"
ssh "${SSH_OPTS[@]}" "$HOST" "mv /tmp/horus-msg-relay.service /etc/systemd/system/horus-msg-relay.service"
ssh "${SSH_OPTS[@]}" "$HOST" "mkdir -p /var/lib/horus"
ssh "${SSH_OPTS[@]}" "$HOST" "systemctl daemon-reload && systemctl enable horus-msg-relay && systemctl restart horus-msg-relay"

echo "==> Verifying..."
sleep 2
ssh "${SSH_OPTS[@]}" "$HOST" "systemctl is-active horus-msg-relay"
ssh "${SSH_OPTS[@]}" "$HOST" "curl -sf http://127.0.0.1:8787/health && echo ' message relay OK'"
ssh "${SSH_OPTS[@]}" "$HOST" "curl -sf https://relay.a10x.eu/health && echo ' public health OK'"

echo "==> Done. Message relay redeployed (7d TTL, lease+ACK)."
echo "    https://relay.a10x.eu"
