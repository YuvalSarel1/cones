#!/bin/bash
# Native queue delivery. Run with --help for task, cancellation and expiry commands.
exec python3 -B "$(dirname "$0")/codex.py" "$@"
