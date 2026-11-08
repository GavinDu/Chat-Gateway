"""CI source guard: keep runtime state scoped to a gateway instance.

This module is picked up by the existing GitHub Actions scripts/test_*.py
unittest discovery. It checks source declarations without compiling the app.
"""

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SOURCE_ROOT = ROOT / "src"
DECLARATION = re.compile(
    r"^\s*(?:(?:pub(?:\([^)]*\))?)\s+)?static\s+"
    r"(?P<mutable>mut\s+)?(?P<name>[A-Za-z_][A-Za-z_0-9]*)\s*:\s*(?P<type>.*)$"
)
MUTABLE_TYPE = re.compile(
    r"\b(?:OnceLock|LazyLock|Mutex|RwLock|RefCell|UnsafeCell|Cell|Atomic[A-Za-z0-9_]*)\b"
)


def disallowed_statics(source):
    """Return (line, declaration) for mutable global statics in Rust source."""
    findings = []
    for number, line in enumerate(source.splitlines(), start=1):
        match = DECLARATION.match(line)
        if match and (match.group("mutable") or MUTABLE_TYPE.search(match.group("type"))):
            findings.append((number, line.strip()))
    return findings


# Existing process globals are tracked as explicit migration debt. The guard
# rejects new mutable statics rather than failing on known pre-existing code.
# RESTART_TEST_SPAWNS is test-only; embedded_login's entries need a follow-up
# owner-scoped runtime migration before removing them from this baseline.
KNOWN_LEGACY_GLOBALS = {
    ("src/browser/pool/supervisor.rs", "RESTART_TEST_SPAWNS"),
    ("src/embedded_login.rs", "REGISTRY"),
    ("src/embedded_login.rs", "CHANNEL"),
}


class ProcessStatePolicyTests(unittest.TestCase):
    def test_no_new_process_global_mutable_state_outside_binary_entrypoint(self):
        found = set()
        for path in sorted(SOURCE_ROOT.rglob("*.rs")):
            # The process entrypoint is excluded; all other modules are
            # guarded by a precise baseline with no wildcard exemptions.
            if path == SOURCE_ROOT / "main.rs":
                continue
            relative_path = path.relative_to(ROOT).as_posix()
            for _, declaration in disallowed_statics(path.read_text(encoding="utf-8")):
                name = declaration.split(":", 1)[0].split()[-1]
                found.add((relative_path, name))
        self.assertEqual(
            found,
            KNOWN_LEGACY_GLOBALS,
            "Mutable Rust statics differ from the reviewed baseline. "
            "New entries must move into AppState/ProviderRuntime; remove "
            "baseline entries when existing globals are migrated.",
        )

    def test_guard_recognizes_legacy_globals_but_allows_immutable_statics(self):
        legacy = (
            "static mut UNSAFE: usize = 0;\n"
            "static STATES: OnceLock<Mutex<HashMap<String, State>>> = OnceLock::new();\n"
            "pub(crate) static BUDGETS: RwLock<Budget> = RwLock::new();\n"
            "static COUNTER: AtomicU64 = AtomicU64::new(0);\n"
            "static MIME: &str = \"application/json\";\n"
        )
        names = [line.split(":")[0].split()[-1] for _, line in disallowed_statics(legacy)]
        self.assertEqual(names, ["UNSAFE", "STATES", "BUDGETS", "COUNTER"])


if __name__ == "__main__":
    unittest.main()
