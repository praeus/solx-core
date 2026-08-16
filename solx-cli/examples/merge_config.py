#!/usr/bin/env python3
"""Seeds solx-config.json's command_actions / allowed_webhook_base_urls
allowlists for the example/test scripts.

solx-cli has no `config` subcommand yet, so this writes the file directly —
a top-level merge, the same shape ConfigService::patch uses.

Usage:
  merge_config.py allow-command <key> <command>
  merge_config.py allow-webhook <prefix>
"""
import json
import os
import sys


def load(path):
    try:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def save(path, doc):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(doc, f)


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2

    appdata = os.environ["SOLX_APPDATA_DIR"]
    path = os.path.join(appdata, "solx-config.json")
    doc = load(path)

    if sys.argv[1] == "allow-command" and len(sys.argv) == 4:
        key, command = sys.argv[2], sys.argv[3]
        doc.setdefault("command_actions", {})[key] = {"command": command}
    elif sys.argv[1] == "allow-webhook" and len(sys.argv) == 3:
        prefix = sys.argv[2]
        doc.setdefault("allowed_webhook_base_urls", []).append(prefix)
    else:
        print(__doc__, file=sys.stderr)
        return 2

    save(path, doc)
    return 0


if __name__ == "__main__":
    sys.exit(main())
