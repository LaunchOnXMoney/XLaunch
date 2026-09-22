#!/usr/bin/env bash
# Start a fresh, isolated mainnet fork; stop only the child we started.
set -euo pipefail
cd "$(dirname "$0")/.."
project_dir="$PWD"
export XLAUNCH_SURFPOOL_PORT="${XLAUNCH_SURFPOOL_PORT:-18999}"
export XLAUNCH_SURFPOOL_WS_PORT="${XLAUNCH_SURFPOOL_WS_PORT:-19000}"
export XLAUNCH_SURFPOOL_STUDIO_PORT="${XLAUNCH_SURFPOOL_STUDIO_PORT:-19488}"
export XLAUNCH_SURFPOOL_URL="http://127.0.0.1:$XLAUNCH_SURFPOOL_PORT"
export XLAUNCH_E2E_EVIDENCE="$project_dir/var/surfpool-e2e/runs"
mkdir -p "$project_dir/var/surfpool-e2e"
surfpool --version
# Do not accidentally connect the tests to somebody else's running fork.
python3 - <<'PY'
import os, socket
sockets = []
for name in ["XLAUNCH_SURFPOOL_PORT", "XLAUNCH_SURFPOOL_WS_PORT", "XLAUNCH_SURFPOOL_STUDIO_PORT"]:
    # create_server uses SO_REUSEADDR on POSIX so recently closed connections
    # do not look like an active listener. SO_REUSEPORT remains disabled.
    sock = socket.create_server(("127.0.0.1", int(os.environ[name])))
    sockets.append(sock)
PY
run_dir="$(mktemp -d "$project_dir/var/surfpool-e2e/runner.XXXXXX")"
surfpool_pid=""
cleanup() {
    if [[ -n "$surfpool_pid" ]]; then
        kill "$surfpool_pid" 2>/dev/null || true
        wait "$surfpool_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
(
    cd "$run_dir"
    exec surfpool start --rpc-url https://api.mainnet.solana.com \
        --host 127.0.0.1 --port "$XLAUNCH_SURFPOOL_PORT" \
        --ws-port "$XLAUNCH_SURFPOOL_WS_PORT" --studio-port "$XLAUNCH_SURFPOOL_STUDIO_PORT" \
        --no-tui --no-studio --no-deploy --airdrop-amount 0 \
        --log-path "$run_dir/logs" --yes
) >"$run_dir/surfpool.log" 2>&1 &
surfpool_pid=$!
export XLAUNCH_SURFPOOL_PID="$surfpool_pid"
if ! python3 - <<'PY'
import json, os, time, urllib.request
deadline = time.monotonic() + 40
while True:
    os.kill(int(os.environ["XLAUNCH_SURFPOOL_PID"]), 0)
    try:
        req = urllib.request.Request(os.environ["XLAUNCH_SURFPOOL_URL"],
            data=json.dumps({"jsonrpc":"2.0","id":1,"method":"getVersion","params":[]}).encode(),
            headers={"Content-Type":"application/json"})
        with urllib.request.urlopen(req, timeout=2) as response:
            version = json.load(response)["result"]
        if version.get("surfnet-version") != "1.5.0":
            raise SystemExit(f"Unverified Surfpool version: {version}")
        print(f"Ready: {version}", flush=True)
        break
    except (OSError, KeyError, ValueError):
        if time.monotonic() >= deadline:
            raise SystemExit("Surfpool startup timed out")
        time.sleep(0.2)
PY
then
    cat "$run_dir/surfpool.log"
    exit 1
fi
cargo test --locked --test surfpool_e2e -- --ignored --nocapture
