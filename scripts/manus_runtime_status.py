#!/usr/bin/env python3
"""Parse the existing loopback relay status without printing untrusted data.

The gateway's enabled/link/auth fields describe *reported* local state.
They never certify a signed-in Manus Chat or Agent task completing.
"""
import json
import sys

MAX_BYTES = 128 * 1024


def _unique_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("duplicate JSON key")
        value[key] = item
    return value


def fixed_manus_status(raw: bytes) -> tuple[str, ...]:
    if not raw or len(raw) > MAX_BYTES:
        raise ValueError("missing or oversized response")
    document = json.loads(raw.decode("utf-8"), object_pairs_hook=_unique_object)
    if (not isinstance(document, dict) or document.get("ok") is not True
            or not isinstance(document.get("providers"), list)):
        raise ValueError("malformed status")
    matches = [
        item for item in document["providers"]
        if isinstance(item, dict) and item.get("provider") == "manus"
    ]
    if len(matches) != 1:
        raise ValueError("Manus status not uniquely present")
    item = matches[0]
    if type(item.get("enabled")) is not bool or type(item.get("linked")) is not bool:
        raise ValueError("untyped Manus status")
    enabled = item["enabled"]
    linked = item["linked"]
    authentication = item.get("authentication")
    if enabled and linked and authentication == "verified":
        reported_auth = "verified by gateway (not live task acceptance)"
    elif enabled and authentication == "needs_login":
        reported_auth = "login needed"
    else:
        reported_auth = "unverified"
    return (
        "Manus running configuration: " + ("enabled" if enabled else "disabled"),
        "Manus saved gateway session: " + ("present" if linked else "absent"),
        "Manus gateway-reported authentication: " + reported_auth,
        "Manus Chat/Agent live acceptance: unverified",
    )


def main() -> int:
    try:
        raw = sys.stdin.buffer.read(MAX_BYTES + 1)
        for line in fixed_manus_status(raw):
            print(line)
    except Exception:
        # Never echo a malformed/older gateway's raw response, origin, account,
        # secret, cookie, exception text or provider page content.
        print("Manus runtime status: unavailable", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
