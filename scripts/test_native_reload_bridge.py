import io
import os
import select
import socket
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
import json
import struct
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
import native_reload_bridge as bridge


class NativeBridgeTests(unittest.TestCase):
    def test_frame_roundtrip(self):
        request = {"v": 1, "id": "a" * 32, "command": "diagnostics"}
        self.assertEqual(bridge.read_frame(io.BytesIO(bridge.encode(request))), request)
        self.assertTrue(bridge.request_valid(request))

    def test_invalid_frames(self):
        for data in (b"", b"\x00", struct.pack("=I", 0),
                     struct.pack("=I", bridge.LIMIT + 1),
                     struct.pack("=I", 8) + b"{}",
                     struct.pack("=I", 2) + b"[]"):
            with self.subTest(data=data), self.assertRaises((EOFError, ValueError)):
                bridge.read_frame(io.BytesIO(data))

    def test_command_allowlist(self):
        base = {"v": 1, "id": "a" * 32, "command": "ping"}
        for change in ({"command": "reload_commit"}, {"command": "shell"},
                       {"v": True}, {"id": "../bad"}, {"args": ["secret"]}):
            self.assertFalse(bridge.request_valid(dict(base, **change)))

    def test_private_directory(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "private"
            with patch.object(bridge, "ROOT", root):
                bridge.private_root()
                self.assertEqual(root.stat().st_mode & 0o777, 0o700)
                root.chmod(0o755)
                with self.assertRaises(PermissionError):
                    bridge.private_root()

    def test_reload_requires_matching_receipt_and_changed_worker(self):
        before, after = "a" * 32, "b" * 32
        seen = []
        def fake_call(command, request_id=None, timeout=20):
            if command == "reload_extension":
                seen.append(request_id)
                return {"ok": True, "result": {"stage": "accepted", "instance": before}}
            seen.append(command)
            if len(seen) == 2:
                return {"ok": True, "result": {"instance": before, "reload_receipt": seen[0]}}
            if len(seen) == 3:
                return {"ok": True, "result": {"instance": after, "reload_receipt": "c" * 32}}
            return {"ok": True, "result": {"instance": after, "reload_receipt": seen[0], "version": "0.3.0"}}
        with patch.object(bridge, "call", fake_call), patch.object(bridge.time, "sleep"):
            result = bridge.reload_verified()
        self.assertTrue(result["ok"])
        self.assertEqual(result["result"]["stage"], "verified")
        self.assertEqual(len(seen), 4)

    def test_rejected_reload_does_not_poll(self):
        with patch.object(bridge, "call", return_value={"ok": False, "error": "busy"}) as call:
            self.assertFalse(bridge.reload_verified()["ok"])
            call.assert_called_once()

    def test_exact_origin_rejected_before_socket_creation(self):
        with self.assertRaises(PermissionError):
            bridge.serve("a" * 32, "chrome-extension://" + "b" * 32 + "/")

    def test_installer_manifest_exact_origin_and_executable(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "private"
            manifest = Path(temp) / "manifests" / "host.json"
            with patch.object(bridge, "ROOT", root), patch.object(bridge, "MANIFEST", manifest), patch.object(bridge.sys, "platform", "darwin"):
                bridge.install("a" * 32)
                data = json.loads(manifest.read_text())
                self.assertEqual(data["allowed_origins"], ["chrome-extension://" + "a" * 32 + "/"])
                self.assertEqual(Path(data["path"]).stat().st_mode & 0o777, 0o700)
                self.assertEqual(manifest.stat().st_mode & 0o777, 0o600)


    def test_reload_deadline_does_not_report_success(self):
        previous = "a" * 32
        ack = {"ok": True, "result": {"stage": "accepted", "instance": previous}}
        with patch.object(bridge, "call", side_effect=[ack, OSError("host gone")]) as call, \
                patch.object(bridge.time, "sleep"), \
                patch.object(bridge.time, "monotonic", side_effect=[0, 1, 2, 76]):
            result = bridge.reload_verified()
        self.assertFalse(result["ok"])
        self.assertEqual(result["error"], "reload_unverified")
        self.assertEqual(call.call_count, 2)

    def test_real_host_subprocess_socket_and_reload_transaction(self):
        # Simulate Chrome's native-message pipes, not an actual Chrome browser.
        with tempfile.TemporaryDirectory(prefix="cg-native-", dir="/tmp") as temp:
            root = Path(temp) / ".chat-gateway-native"
            sock = root / "bridge.sock"
            env = dict(os.environ, HOME=temp)
            extension_id = "a" * 32
            cmd = [sys.executable, "-u", str(Path(bridge.__file__).resolve()),
                   "host", extension_id, "chrome-extension://" + extension_id + "/"]
            proc = subprocess.Popen(cmd, env=env, stdin=subprocess.PIPE,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
            try:
                deadline = time.monotonic() + 5
                while not sock.exists() and proc.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertTrue(sock.is_socket(), "native host failed to bind")
                # bind precedes chmod; private directory already protects access.
                self.assertEqual(root.stat().st_mode & 0o777, 0o700)
                self.assertEqual(sock.stat().st_mode & 0o777, 0o600)
                contender = subprocess.run(cmd, env=env, input=b"",
                                           capture_output=True, timeout=5)
                self.assertNotEqual(contender.returncode, 0)
                self.assertTrue(sock.is_socket(), "second host removed active socket")

                def receive():
                    self.assertTrue(select.select([proc.stdout], [], [], 5)[0],
                                    "native host did not emit a frame")
                    return bridge.read_frame(proc.stdout)

                def respond(request, result):
                    proc.stdin.write(bridge.encode({"v": 1, "id": request["id"],
                                                     "ok": True, "result": result}))
                    proc.stdin.flush()

                def client_call(command, request_id):
                    request = {"v": 1, "id": request_id, "command": command}
                    with socket.socket(socket.AF_UNIX) as client:
                        client.settimeout(8)
                        client.connect(str(sock))
                        client.sendall(bridge.encode(request))
                        with client.makefile("rb") as incoming:
                            return bridge.read_frame(incoming)

                # Reject an invalid client without forwarding it to Chrome.
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(3)
                    client.connect(str(sock))
                    client.sendall(struct.pack("=I", bridge.LIMIT + 1))
                    self.assertEqual(client.recv(1), b"")

                ids = ["b" * 32, "c" * 32]
                with ThreadPoolExecutor(max_workers=2) as pool:
                    futures = [pool.submit(client_call, "ping", ident) for ident in ids]
                    seen = set()
                    for _ in ids:
                        request = receive()
                        self.assertEqual(request["command"], "ping")
                        seen.add(request["id"])
                        respond(request, {"echo": request["id"]})
                    self.assertEqual(seen, set(ids))
                    for ident, future in zip(ids, futures):
                        self.assertEqual(future.result(timeout=8)["result"]["echo"], ident)

                    reload_id = "d" * 32
                    future = pool.submit(client_call, "reload_extension", reload_id)
                    prepare = receive()
                    self.assertEqual(prepare["command"], "reload_extension")
                    self.assertEqual(prepare["id"], reload_id)
                    # No internal commit is emitted without Chrome's ack.
                    self.assertFalse(select.select([proc.stdout], [], [], 0.05)[0])
                    respond(prepare, {"stage": "accepted", "instance": "e" * 32})
                    self.assertEqual(future.result(timeout=8)["result"]["stage"], "accepted")
                    self.assertEqual(receive(), {"v": 1, "id": reload_id, "command": "reload_commit"})
            finally:
                proc.stdin.close()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)
                proc.stdout.close()
                proc.stderr.close()
            self.assertEqual(proc.returncode, 0)
            self.assertFalse(sock.exists(), "native host left a stale socket")


if __name__ == "__main__":
    unittest.main()
