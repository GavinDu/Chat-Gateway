"""Verify the carried native crates and review diff against the pinned checkout."""
import difflib
import hashlib
import json
from pathlib import Path
import sys
import tomllib

root = Path(__file__).resolve().parents[1]
upstream = Path(sys.argv[1])
vendor = root / "vendor/obscura-native"
names = {"obscura-js", "obscura-net", "obscura-browser"}
manifest = json.loads((vendor / "UPSTREAM_SHA256.json").read_text())
actual = {
    str(path.relative_to(vendor))
    for name in names for path in (vendor / name).rglob("*") if path.is_file()
}
expected = {
    str(path.relative_to(upstream / "crates"))
    for name in names for path in (upstream / "crates" / name).rglob("*") if path.is_file()
}
assert actual == expected == set(manifest), "native source inventory differs"
patch = []
for relative in sorted(manifest):
    before = (upstream / "crates" / relative).read_bytes()
    after = (vendor / relative).read_bytes()
    assert hashlib.sha256(before).hexdigest() == manifest[relative], relative
    if before != after:
        patch.extend(difflib.unified_diff(
            before.decode().splitlines(True), after.decode().splitlines(True),
            fromfile="a/" + relative, tofile="b/" + relative))
assert "".join(patch) == (vendor / "changes.patch").read_text(), "native review patch differs"
config = tomllib.loads((root / "Cargo.toml").read_text())
revision = config["dependencies"]["obscura-core"]["rev"]
assert revision == "0e2ee76383b95a14e51203799c4abc6790d6c7df"
packaging = tomllib.loads((vendor / "Cargo.toml").read_text())
assert set(packaging["workspace"]["members"]) == names
for name in names:
    assert config["patch"]["https://github.com/h4ckf0r0day/obscura"][name]["path"] == f"vendor/obscura-native/{name}"
for name, entry in packaging["workspace"]["dependencies"].items():
    if name.startswith("obscura-") and name not in names:
        assert entry["rev"] == revision and entry["git"] == "https://github.com/h4ckf0r0day/obscura"
assert (vendor / "LICENSE").read_bytes() == (upstream / "LICENSE").read_bytes()
print(f"verified {len(actual)} pinned native source files and exact review patch")
