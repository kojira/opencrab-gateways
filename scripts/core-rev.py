#!/usr/bin/env python3
"""Print the single kojira/opencrab core rev this workspace is locked to.

Cargo.lock is the source of truth for the core the gateways were tested
against. Exactly one core rev must be locked; anything else fails.
"""

import pathlib
import re
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = re.compile(r"^git\+https://github\.com/kojira/opencrab\?rev=([0-9a-f]{40})#([0-9a-f]{40})$")


def main() -> int:
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    revs = set()
    for package in lock.get("package", []):
        source = package.get("source", "")
        if "github.com/kojira/opencrab" not in source:
            continue
        match = SOURCE.match(source)
        if not match or match.group(1) != match.group(2):
            print(f"unexpected core source: {source}", file=sys.stderr)
            return 1
        revs.add(match.group(1))
    if len(revs) != 1:
        print(f"expected exactly one locked core rev, got {sorted(revs)}", file=sys.stderr)
        return 1
    print(revs.pop())
    return 0


if __name__ == "__main__":
    sys.exit(main())
