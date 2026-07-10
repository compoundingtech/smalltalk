#!/bin/bash
# st PreCompact hook — fail-open shim (brief-024 hook-legs).
#
# PRIME DIRECTIVE: never block compaction. Claude Code runs PreCompact
# under macOS /bin/bash (3.2), where a hook that fails to PARSE
# fail-CLOSES — compaction is blocked, which wedges the whole session.
# That is the worst possible outcome, and a parse error can't be caught
# from inside the file that fails to parse.
#
# So the real logic lives in `pre-compact.impl.sh` and this entrypoint
# runs it as a SUBPROCESS under `|| true`: a parse error, crash, or hang
# in the logic can never fail this hook. Keep THIS file trivially
# parse-safe — no heredocs, no nested command substitution — so it
# cannot itself fail to parse under bash 3.2.

impl="$(dirname "$0")/pre-compact.impl.sh"
if [ -r "$impl" ]; then
  # Run the real logic; swallow ANY failure (parse/exec/timeout) so it
  # can never propagate a non-zero status and block compaction.
  /bin/bash "$impl" "$@" || true
fi

# The prime directive: never block compaction. exit 0 unconditionally.
exit 0
