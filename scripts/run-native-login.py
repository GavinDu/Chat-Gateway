#!/usr/bin/env python3
"""Private, loopback-only launcher for the supported native login runtime.

Never reads provider/session files or secret values. The gateway inherits its
vault key from the runtime environment; only the non-secret API-key hash is
used to construct configuration.
"""
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT
STATE = Path(os.environ.get("CHAT_GATEWAY_NATIVE_STATE_DIR", str(ROOT / "data/native-login"))).resolve()
BINARY = Path(os.environ.get("CHAT_GATEWAY_NATIVE_BINARY", str(ROOT / "target/debug/chat-gateway-rs"))).resolve()
PROVIDERS = {"chatgpt", "claude", "gemini", "grok", "qwen", "metaai", "mistral", "perplexity"}

def plan(environment):
    """Only membership tests for secret keys; only public settings are read."""
    missing = [name for name in ("CHAT_GATEWAY_VAULT_KEY",) if name not in environment]
    digest = environment.get("CHAT_GATEWAY_AUTH_SECRET_HASH", "")
    from_env = not digest and "CHAT_GATEWAY_API_KEY" in environment
    if digest and not re.fullmatch(r"[a-fA-F0-9]{64}", digest):
        missing.append("CHAT_GATEWAY_AUTH_SECRET_HASH (64-character SHA-256 digest)")
    elif not digest and not from_env:
        missing.append("CHAT_GATEWAY_API_KEY")
    if "CHAT_GATEWAY_REMOTE_ORIGIN" in environment:
        missing.append("CHAT_GATEWAY_REMOTE_ORIGIN must be absent for loopback login")
    enabled = environment.get("CHAT_GATEWAY_ENABLED_PROVIDERS", "")
    providers = [p.strip() for p in enabled.split(",") if p.strip()]
    if len(set(providers)) != len(providers) or any(p not in PROVIDERS for p in providers):
        missing.append("CHAT_GATEWAY_ENABLED_PROVIDERS contains duplicate/unsupported names")
    return missing, digest, from_env, providers

def main():
    missing, digest, from_env, providers = plan(os.environ)
    if "--diagnose" in sys.argv:
        print(json.dumps({
            "bind": "127.0.0.1:8099", "remote_login": False,
            "secret_presence": {name: name in os.environ for name in
                                ("CHAT_GATEWAY_VAULT_KEY", "CHAT_GATEWAY_API_KEY")},
            "owner_auth_source": "gateway_process_environment" if from_env else "configured_hash",
            "enabled_providers": providers if not missing else [],
            "missing_or_invalid_names": missing,
            "binary_present": BINARY.is_file(),
            "state_location": str(STATE),
        }))
        return 2 if missing else 0
    if missing:
        print("Embedded login is blocked; required configuration names: " +
              ", ".join(missing), flush=True)
        return 2
    binary = BINARY
    if not binary.is_file():
        print("Build required: cargo build --locked (from this checkout)", flush=True)
        return 2
    os.umask(0o077)
    for path in (STATE, STATE / "home", STATE / "engine", STATE / "sessions"):
        if path.is_symlink():
            print("Refusing a symlinked private state directory.", flush=True)
            return 2
        path.mkdir(mode=0o700, parents=True, exist_ok=True)
        path.chmod(0o700)
    config = {
        "auth": {"keys": [] if from_env else [{"name": "embedded-login-owner",
                          "secret_hash": digest.lower(),
                          "scopes": ["admin", "chat"]}]},
        "server": {"port": 8099, "chrome_profile_dir": str(STATE / "engine")},
        "obscura": {"storage_dir": str(STATE / "engine")},
        # Login is available before routing providers is explicitly enabled.
        "providers": [{"name": name, "enabled": True} for name in providers],
    }
    config_path = STATE / "config.yaml"
    config_path.write_text(json.dumps(config))
    config_path.chmod(0o600)
    os.environ.update({
        "PORT": "8099", "HOME": str(STATE / "home"),
        "CHAT_GATEWAY_DOCKER_LOCAL_BIND": "0",
        "CHAT_GATEWAY_SESSION_STORE": "vault",
        "CHAT_GATEWAY_MEMORY_PATH": str(STATE / "conversations.sqlite3"),
        "CHAT_GATEWAY_LEGACY_LOGIN_RELAY": "0",
        "RUST_LOG": "off",
        "CHAT_GATEWAY_EMBEDDED_BROWSER": "enabled",
    })
    # Scope the Nix static-TLS workaround to Rust, never the browser worker.
    nix_library = "/nix/store/2ga5nd1m56n5cx2wh8vbf6nrdhqk2f0q-gcc-15.3.0-lib/lib"
    if Path(nix_library).is_dir():
        os.environ.update(GLIBC_TUNABLES="glibc.rtld.optional_static_tls=1048576",
                          LD_LIBRARY_PATH=nix_library)
    if Path("/repl/tools/bin/chromium").is_file():
        os.environ.setdefault("CHAT_GATEWAY_CHROMIUM_PATH", "/repl/tools/bin/chromium")
    if from_env:
        os.environ["CHAT_GATEWAY_OWNER_AUTH_FROM_ENV"] = "1"
    else:
        os.environ.pop("CHAT_GATEWAY_OWNER_AUTH_FROM_ENV", None)
    child = subprocess.Popen(
        [str(binary), "--config", str(config_path)], cwd=STATE,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, lambda number, _frame: child.send_signal(number))
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    deadline = time.monotonic() + 45
    healthy = False
    while child.poll() is None and time.monotonic() < deadline:
        try:
            with opener.open("http://127.0.0.1:8099/health", timeout=1) as response:
                healthy = response.status == 200
                if healthy:
                    break
        except Exception:
            time.sleep(0.1)
    if not healthy:
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        print("Gateway initialization failed. Verify CHAT_GATEWAY_VAULT_KEY format, "
              "CHAT_GATEWAY_API_KEY format, storage permissions and runtime dependencies; "
              "no secret values or raw gateway logs are displayed.", flush=True)
        return 1
    print("Health HTTP 200. Embedded login: http://127.0.0.1:8099/login", flush=True)
    print("Loopback only; no public mapping. Enable routing explicitly with "
          "CHAT_GATEWAY_ENABLED_PROVIDERS after owner approval.", flush=True)
    return child.wait()


if __name__ == "__main__":
    sys.exit(main())
