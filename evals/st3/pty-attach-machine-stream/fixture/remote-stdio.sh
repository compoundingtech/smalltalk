#!/usr/bin/env bash
set -euo pipefail

exec env -u PTY_SESSION PTY_ROOT="${PTY_REMOTE_ROOT:?PTY_REMOTE_ROOT must be set}" pty remote-serve --stdio
