#!/usr/bin/env python3
"""Tiny JSON field extractor for the bash example/test scripts.

Reads a JSON document on stdin and a dotted path (e.g. `result.title`,
`items.0.id`) as argv[1], and prints the value at that path. Strings print
raw (no quotes); anything else prints as compact JSON. Prints nothing and
exits 1 if the path doesn't resolve, so callers can test with `[ -z "$v" ]`.
"""
import json
import sys

def main() -> int:
    if len(sys.argv) != 2:
        print("usage: jget.py <dotted.path> < json", file=sys.stderr)
        return 2
    path = sys.argv[1]
    doc = json.loads(sys.stdin.read())
    cur = doc
    for part in path.split("."):
        if isinstance(cur, list):
            cur = cur[int(part)]
        elif isinstance(cur, dict):
            if part not in cur:
                return 1
            cur = cur[part]
        else:
            return 1
    if isinstance(cur, str):
        print(cur)
    elif cur is None:
        print("null")
    else:
        print(json.dumps(cur))
    return 0

if __name__ == "__main__":
    sys.exit(main())
