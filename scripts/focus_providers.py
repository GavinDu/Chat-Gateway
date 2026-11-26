#!/usr/bin/env python3
"""Owner-run, reversible provider focus for an existing local Chat-Gateway config.

Only provider enabled booleans change. Never print credentials, copy config
content into a backup or restart Docker automatically. ChatGPT and Meta AI
are *not* automatically enabled: review account authorization first.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import stat
import tempfile

FOCUS = frozenset(("chatgpt", "metaai"))
SUPPORTED = frozenset((
    "qwen", "metaai", "claude", "chatgpt", "gemini", "grok",
    "mistral", "perplexity", "manus",
))
PROVIDER_HEADER = re.compile(r"^providers:\s*(?:#.*)?$")
ITEM = re.compile(r"^  - name: ([a-z][a-z0-9_-]*)(?:[ \t]+#.*)?$")
ENABLED = re.compile(
    r"^(?P<prefix>    enabled:[ \t]*)(?P<value>true|false)"
    r"(?P<suffix>[ \t]*(?:#.*)?)$"
)
MAX_CONFIG_BYTES = 1_048_576


class FocusError(Exception):
    """Fixed, non-sensitive category; never include config content or paths."""


def _regular_private_file(path: Path) -> None:
    try:
        info = path.lstat()
    except OSError:
        raise FocusError("config_missing") from None
    if not stat.S_ISREG(info.st_mode) or path.is_symlink():
        raise FocusError("unsafe_config_path")
    if info.st_size > MAX_CONFIG_BYTES:
        raise FocusError("config_too_large")


def _load(path: Path) -> str:
    _regular_private_file(path)
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeError):
        raise FocusError("config_unreadable") from None


def _parse(text: str) -> tuple[list[str], dict[str, tuple[int, bool, re.Match[str]]]]:
    lines = text.splitlines(keepends=True)
    # This intentionally supports only standard config.yaml provider layout.
    # Fail closed on dynamic aliases, inline maps, missing explicit enabled or
    # duplicate names instead of changing a different section.
    entries: dict[str, tuple[int, bool, re.Match[str]]] = {}
    sections = 0
    inside = False
    current: str | None = None
    seen = set()
    for index, original in enumerate(lines):
        line = original.rstrip("\r\n")
        if line and not line[0].isspace() and not line.startswith("#"):
            if PROVIDER_HEADER.fullmatch(line):
                inside = True
                sections += 1
            else:
                inside = False
            current = None
        if not inside:
            continue
        if line.startswith("  - "):
            item = ITEM.fullmatch(line)
            if item is None:
                raise FocusError("unsupported_provider_layout")
            current = item.group(1)
            if current not in SUPPORTED or current in seen:
                raise FocusError("unknown_or_duplicate_provider")
            seen.add(current)
        elif line.startswith("    enabled:"):
            match = ENABLED.fullmatch(line)
            if current is None or current in entries or match is None:
                raise FocusError("invalid_enabled_field")
            entries[current] = (index, match.group("value") == "true", match)
    if sections != 1 or not FOCUS.issubset(seen) or set(entries) != seen:
        raise FocusError("incomplete_provider_layout")
    return lines, entries


def _render(text: str, flags: dict[str, bool]) -> str:
    lines, entries = _parse(text)
    if set(flags) != set(entries):
        raise FocusError("provider_inventory_changed")
    for name, (line_number, _, match) in entries.items():
        original = lines[line_number]
        tail = original[len(original.rstrip("\r\n")):]
        lines[line_number] = (
            match.group("prefix") + ("true" if flags[name] else "false")
            + match.group("suffix") + tail
        )
    return "".join(lines)


def _write_atomic(path: Path, contents: str) -> None:
    # Never dump original config into logs or a temporary, world-readable
    # copy; secure same-directory replacement maintains strict 0600 mode.
    fd, temporary = tempfile.mkstemp(prefix=".cg-provider-focus-", dir=str(path.parent))
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8", newline="") as stream:
            stream.write(contents)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _manifest_path(config: Path) -> Path:
    return config.with_name(config.name + ".focus-flags.json")


def _read_manifest(path: Path) -> dict[str, bool]:
    _regular_private_file(path)
    try:
        record = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeError):
        raise FocusError("invalid_focus_manifest") from None
    flags = record.get("flags") if isinstance(record, dict) and record.get("version") == 1 else None
    if not isinstance(flags, dict) or not all(
        isinstance(k, str) and k in SUPPORTED and type(v) is bool
        for k, v in flags.items()
    ):
        raise FocusError("invalid_focus_manifest")
    return flags


def operate(config: Path, mode: str) -> dict[str, object]:
    raw = _load(config)
    _, entries = _parse(raw)
    flags = {name: item[1] for name, item in entries.items()}
    non_target = sorted(set(flags) - FOCUS)
    manifest = _manifest_path(config)
    # Reject manifest symlinks, even on dry runs.
    if manifest.is_symlink():
        raise FocusError("unsafe_manifest_path")
    if mode == "restore":
        old = _read_manifest(manifest)
        if set(old) != set(non_target):
            raise FocusError("provider_inventory_changed")
        if any(flags[name] for name in non_target):
            raise FocusError("focus_state_changed")
        # Restore *only* flags this focus mode changed. The owner's manual
        # ChatGPT/Meta AI changes during the focus period must survive.
        restored_flags = {name: (flags[name] if name in FOCUS else old[name]) for name in flags}
        restored = _render(raw, restored_flags)
        _write_atomic(config, restored)
        manifest.unlink()
        return {"state": "restored", "previously_enabled": sorted(
            name for name in old if old[name] and name not in FOCUS
        )}
    disabled = [name for name in non_target if flags[name]]
    if mode == "apply":
        if manifest.exists():
            old = _read_manifest(manifest)
            if set(old) != set(non_target):
                raise FocusError("provider_inventory_changed")
        else:
            # Only the seven flags subject to modification need a snapshot.
            # No credentials, target flags or unrelated configuration included.
            _write_atomic(manifest, json.dumps({"version": 1, "flags": {
                name: flags[name] for name in non_target
            }}, sort_keys=True) + "\n")
        target_flags = {name: (flags[name] if name in FOCUS else False) for name in flags}
        modified = _render(raw, target_flags)
        if modified != raw:
            _write_atomic(config, modified)
    return {
        "state": "applied" if mode == "apply" else "dry_run",
        "disabled": disabled,
        "chatgpt": "enabled" if flags["chatgpt"] else "disabled_unchanged",
        "metaai": "enabled" if flags["metaai"] else "disabled_unchanged",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=Path(__file__).resolve().parent.parent / "config.yaml")
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--apply", action="store_true")
    group.add_argument("--restore", action="store_true")
    args = parser.parse_args()
    mode = "restore" if args.restore else ("apply" if args.apply else "dry_run")
    try:
        result = operate(args.config, mode)
    except (FocusError, OSError) as error:
        # Category only; never expose path, provider cookies, or YAML text.
        print(json.dumps({"state": "refused", "reason": error.args[0] if isinstance(error, FocusError) else "filesystem_failure"}))
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
