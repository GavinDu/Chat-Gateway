import io
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


if __name__ == "__main__":
    unittest.main()
