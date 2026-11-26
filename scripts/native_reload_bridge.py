#!/usr/bin/env python3
"""Owner-local Chrome native messaging host and CLI. Standard library only."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import queue
import re
import shlex
import socket
import stat
import struct
import sys
import tempfile
import threading
import time
import uuid

HOST = "com.chat_gateway.reload_bridge"
ROOT = Path.home() / ".chat-gateway-native"
SOCKET = ROOT / "bridge.sock"
MANIFEST = Path.home() / "Library/Application Support/Google/Chrome/NativeMessagingHosts" / (HOST + ".json")
LIMIT = 65536
COMMANDS = {"ping", "diagnostics", "reload_extension"}
ID = re.compile(r"[a-f0-9]{32}\Z")
EXT_ID = re.compile(r"[a-p]{32}\Z")


def read_exact(stream, size):
    data = bytearray()
    while len(data) < size:
        part = stream.read(size - len(data))
        if not part:
            raise EOFError("frame ended")
        data.extend(part)
    return bytes(data)


def read_frame(stream):
    size = struct.unpack("=I", read_exact(stream, 4))[0]
    if not 0 < size <= LIMIT:
        raise ValueError("invalid frame size")
    obj = json.loads(read_exact(stream, size))
    if not isinstance(obj, dict):
        raise ValueError("invalid frame")
    return obj


def encode(obj):
    raw = json.dumps(obj, separators=(",", ":"), allow_nan=False).encode("utf-8")
    if not 0 < len(raw) <= LIMIT:
        raise ValueError("invalid frame size")
    return struct.pack("=I", len(raw)) + raw


def request_valid(req):
    return (set(req) == {"v", "id", "command"} and type(req["v"]) is int
            and req["v"] == 1 and isinstance(req["id"], str)
            and ID.fullmatch(req["id"]) and isinstance(req["command"], str)
            and req["command"] in COMMANDS)


def private_root():
    ROOT.mkdir(mode=0o700, exist_ok=True)
    info = ROOT.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise PermissionError("unsafe bridge directory")


def atomic_write(path, text, mode):
    fd, temp = tempfile.mkstemp(dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as out:
            os.fchmod(out.fileno(), mode)
            out.write(text)
        os.replace(temp, path)
    finally:
        if os.path.exists(temp):
            os.unlink(temp)


def install(extension_id):
    if sys.platform != "darwin" or not EXT_ID.fullmatch(extension_id):
        raise ValueError("install requires macOS and an exact Chrome extension ID")
    private_root()
    script = ROOT / "bridge.py"
    launcher = ROOT / "launch-host"
    atomic_write(script, Path(__file__).read_text(encoding="utf-8"), 0o600)
    atomic_write(launcher, "#!/bin/sh\nexec " + shlex.quote(sys.executable) + " " +
                 shlex.quote(str(script)) + " host " + shlex.quote(extension_id) + ' "$@"\n', 0o700)
    MANIFEST.parent.mkdir(parents=True, exist_ok=True)
    atomic_write(MANIFEST, json.dumps({"name": HOST, "description": "Chat Gateway local development bridge",
                 "path": str(launcher), "type": "stdio",
                 "allowed_origins": ["chrome-extension://" + extension_id + "/"]}, indent=2) + "\n", 0o600)


def error_reply(req, code):
    return {"v": 1, "id": req["id"], "ok": False, "error": code}


def serve(extension_id, origin):
    if not EXT_ID.fullmatch(extension_id) or origin not in {"chrome-extension://" + extension_id, "chrome-extension://" + extension_id + "/"}:
        raise PermissionError("wrong extension origin")
    private_root()
    os.umask(0o077)
    # One browser profile at a time. Never unlink another live listener.
    fd = os.open(ROOT / "bridge.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if SOCKET.exists() or SOCKET.is_symlink():
            if not stat.S_ISSOCK(SOCKET.lstat().st_mode):
                raise PermissionError("unsafe socket path")
            SOCKET.unlink()
        with socket.socket(socket.AF_UNIX) as server:
            server.bind(str(SOCKET))
            try:
                os.chmod(SOCKET, 0o600)
                server.listen(4)
                server.settimeout(0.5)
                responses = queue.Queue(maxsize=8)
                stopped = threading.Event()

                def read_native():
                    try:
                        while True:
                            responses.put_nowait(read_frame(sys.stdin.buffer))
                    except (EOFError, ValueError, OSError, queue.Full):
                        stopped.set()

                threading.Thread(target=read_native, daemon=True).start()

                def send_native(obj):
                    sys.stdout.buffer.write(encode(obj))
                    sys.stdout.buffer.flush()

                while not stopped.is_set():
                    try:
                        client, _ = server.accept()
                    except socket.timeout:
                        continue
                    with client:
                        client.settimeout(5)
                        try:
                            with client.makefile("rb") as incoming:
                                req = read_frame(incoming)
                            if not request_valid(req):
                                continue
                            send_native(req)
                            deadline = time.monotonic() + 15
                            response = None
                            while not stopped.is_set() and time.monotonic() < deadline:
                                try:
                                    candidate = responses.get(timeout=0.25)
                                except queue.Empty:
                                    continue
                                if candidate.get("v") == 1 and candidate.get("id") == req["id"]:
                                    response = candidate
                                    break
                            if response is None:
                                client.sendall(encode(error_reply(req, "worker_timeout")))
                                continue
                            if response.get("ok") is True and req["command"] == "reload_extension":
                                result = response.get("result")
                                if not isinstance(result, dict) or result.get("stage") != "accepted" or not ID.fullmatch(str(result.get("instance", ""))):
                                    client.sendall(encode(error_reply(req, "invalid_ack")))
                                    continue
                                # Only commit after the acknowledgement is handed to the CLI.
                                client.sendall(encode(response))
                                send_native({"v": 1, "id": req["id"], "command": "reload_commit"})
                            else:
                                client.sendall(encode(response))
                        except (EOFError, ValueError, OSError):
                            continue
            finally:
                SOCKET.unlink(missing_ok=True)


def call(command, request_id=None, timeout=20):
    req = {"v": 1, "id": request_id or uuid.uuid4().hex, "command": command}
    private_root()
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(timeout)
        client.connect(str(SOCKET))
        client.sendall(encode(req))
        with client.makefile("rb") as incoming:
            reply = read_frame(incoming)
    if reply.get("v") != 1 or reply.get("id") != req["id"]:
        raise ValueError("invalid reply")
    return reply


def reload_verified():
    request_id = uuid.uuid4().hex
    ack = call("reload_extension", request_id)
    if ack.get("ok") is not True:
        return ack
    previous = ack["result"]["instance"]
    deadline = time.monotonic() + 75
    while time.monotonic() < deadline:
        time.sleep(0.5)
        try:
            pong = call("ping", timeout=min(2, max(0.1, deadline - time.monotonic())))
            result = pong.get("result", {})
            if (pong.get("ok") is True and result.get("instance") != previous
                    and ID.fullmatch(str(result.get("instance", "")))
                    and result.get("reload_receipt") == request_id):
                return {"v": 1, "id": request_id, "ok": True, "result": {
                    "stage": "verified", "previous_instance": previous,
                    "instance": result["instance"], "version": result["version"]}}
        except (OSError, EOFError, ValueError):
            pass
    return {"v": 1, "id": request_id, "ok": False, "error": "reload_unverified"}


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    ins = sub.add_parser("install")
    ins.add_argument("extension_id")
    sub.add_parser("uninstall")
    native = sub.add_parser("host")
    native.add_argument("extension_id")
    native.add_argument("origin")
    native.add_argument("chrome_args", nargs=argparse.REMAINDER)
    for command in sorted(COMMANDS):
        sub.add_parser(command)
    args = parser.parse_args()
    try:
        if args.command == "host":
            serve(args.extension_id, args.origin)
            return 0
        if args.command == "install":
            install(args.extension_id)
            print("Native host registered. Extension 0.3.1+ connects automatically unless the local bridge is disabled. Older versions require manual enablement.")
            return 0
        if args.command == "uninstall":
            if sys.platform != "darwin":
                raise ValueError("macOS only")
            MANIFEST.unlink(missing_ok=True)
            print("Registration removed. Disable the local bridge in Extension options to stop a live connection.")
            return 0
        reply = reload_verified() if args.command == "reload_extension" else call(args.command)
        print(json.dumps(reply, indent=2))
        return 0 if reply.get("ok") is True else 1
    except (OSError, EOFError, ValueError, KeyError, TypeError):
        print("Native bridge unavailable or invalid response.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
