"""Exercise the compiled Rust receiver with SIGKILL and real HTTP POSTs."""
import json
import socket
import subprocess
import tempfile
import time
from pathlib import Path
from urllib.error import URLError
from urllib.request import Request, urlopen


project = Path(__file__).resolve().parents[1]
binary = project / "target/debug/capture_ingest"


def free_address():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return f"127.0.0.1:{sock.getsockname()[1]}"


def start(root, log):
    address = free_address()
    proc = subprocess.Popen([str(binary), address, str(root)], stdout=log, stderr=log)
    endpoint = f"http://{address}"
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise AssertionError(f"receiver exited with {proc.returncode}")
        try:
            with urlopen(endpoint + "/healthz", timeout=1) as response:
                assert response.status == 200
                return proc, endpoint
        except URLError:
            time.sleep(0.05)
    proc.kill()
    proc.wait(timeout=5)
    raise AssertionError("receiver did not start")


def post(endpoint, payment):
    req = Request(endpoint + "/transactions", data=json.dumps(payment).encode(),
                  headers={"Content-Type": "application/json"})
    with urlopen(req, timeout=10) as response:
        assert response.status == 200
        assert json.load(response) == {"ok": True}


def read_state(root):
    return json.loads((root / "state.json").read_text(), parse_float=str)


with tempfile.TemporaryDirectory(prefix="xlaunch-recovery-") as temporary:
    root = Path(temporary) / "state"
    proc = None
    with (Path(temporary) / "receiver.log").open("w+") as log:
        try:
            first = {"sender": "@crash-test", "amount": "12.34", "memo": "before crash\n🪙"}
            second = {"sender": "@after-restart", "amount": "25.50", "memo": "after crash"}
            proc, endpoint = start(root, log)
            post(endpoint, first)
            before_crash = read_state(root)
            assert before_crash["transactions"][0]["payment"] == first
            assert before_crash["next_sequence"] == 2
            proc.kill()
            assert proc.wait(timeout=5) == -9

            # A process dying before atomic replacement can leave an incomplete temp file.
            (root / ".state-interrupted.tmp").write_text('{"transactions":[')
            proc, endpoint = start(root, log)
            assert read_state(root) == before_crash
            post(endpoint, second)
            recovered = read_state(root)
            assert recovered["transactions"][0] == before_crash["transactions"][0]
            assert recovered["transactions"][1]["payment"] == second
            assert [r["sequence"] for r in recovered["transactions"]] == [1, 2]
            assert recovered["next_sequence"] == 3
            proc.kill()
            assert proc.wait(timeout=5) == -9

            proc, endpoint = start(root, log)
            assert read_state(root) == recovered
            print(json.dumps({"sigkill_restarts": 2, "acknowledged_payments_recovered": 2,
                              "sequence_continues": True, "incomplete_temp_ignored": True,
                              "status": "passed"}))
        except BaseException:
            log.flush()
            log.seek(0)
            print(log.read())
            raise
        finally:
            if proc is not None and proc.poll() is None:
                proc.kill()
                proc.wait(timeout=5)
