#!/usr/bin/env python3
"""Keep a caller's live Herdr session out of repository test processes."""

import os
import sys


def main():
    if len(sys.argv) < 2:
        print("usage: run_isolated_tests.py <command> [args ...]", file=sys.stderr)
        return 64
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith("HERDR_")}
    try:
        os.execvpe(sys.argv[1], sys.argv[1:], environment)
    except OSError as error:
        print(f"run_isolated_tests: {error}", file=sys.stderr)
        return 69


if __name__ == "__main__":
    sys.exit(main())
