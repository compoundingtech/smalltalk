#!/bin/bash
# st StopFailure hook — surfaces API-error wedges via st,
# with a noise level tuned to the underlying error_type.
#
# Why this exists: when a Claude Code session ends mid-turn because of an
# Anthropic API error (rate-limit, auth failure, billing, transient 5xx),
# the agent stops responding and there's no obvious signal except "the
# session went quiet." operator's call is no auto-restart — just clear
# visibility so a human can decide whether to nudge the session or wait
# for the upstream condition to resolve. This hook is the visibility leg.
#
# Policy by error_type:
#
#   rate_limit            status=away. NO ding. Transient — Anthropic
#                         capacity events would otherwise spam operator with
#                         one ping per wedged agent.
#   server_error          status=away + ding operator. Worth a heads-up; not
#                         urgent (recovers on its own).
#   authentication_failed status=offline + URGENT ding (priority high).
#   oauth_org_not_allowed   These need a human; the agent cannot recover.
#   billing_error
#   max_output_tokens     IGNORE. Programmer error / long turn — not
#   invalid_request       infrastructure. The agent itself, not st,
#   model_not_found       owns recovering from these.
#   <anything else>       status=away + ding operator with the error_type
#                         verbatim, so we learn what new failure modes
#                         emerge and can tune the table.
#
# Install per examples/claude-code/README.md. Reads hook envelope
# JSON from stdin; emits no required stdout (Claude Code ignores it).

set -uo pipefail

# ─── Binary resolution ───────────────────────────────────────────────────
#
# Prefer an absolute path injected by `st launch` via $ST_BIN. Falls
# back to PATH lookup for users who wired settings.local.json by hand.
# `st` is the canonical binary.
st_bin="${ST_BIN:-}"
if [[ -z "$st_bin" ]]; then
  st_bin="$(command -v st 2>/dev/null || true)"
fi

# Read the JSON envelope Claude Code pipes in.
input="$(cat)"

# Identity comes from the agent's spawn env (see brief-027 onboarding doc:
# pty.toml's `[sessions.claude.env] ST_AGENT = "..."`).
identity="${ST_AGENT:-}"

if [[ -z "$identity" ]]; then
  # No identity → nothing useful we can report. Exit silently; Claude Code
  # ignores our exit code anyway.
  exit 0
fi

if [[ -z "$st_bin" ]]; then
  # No CLI on PATH and no $ST_BIN — we can't ding or change status.
  # Silent exit; Claude Code ignores our exit code.
  exit 0
fi

# Extract error_type via jq. Falls back to "unknown" if the field is
# missing or jq fails to parse the envelope.
error_type="$(printf '%s' "$input" | jq -r '.error_type // "unknown"' 2>/dev/null || echo unknown)"

case "$error_type" in
  rate_limit)
    "$st_bin" status "$identity" --set away
    ;;

  server_error)
    "$st_bin" status "$identity" --set away
    "$st_bin" message send operator \
      --subject "agent ${identity} wedged: server_error" \
      -m "Agent ${identity} ended a turn on server_error (transient Anthropic-side issue). Status set to away. No action required unless the wedge persists."
    ;;

  authentication_failed | oauth_org_not_allowed)
    "$st_bin" status "$identity" --set offline
    "$st_bin" message send operator \
      --priority high \
      --subject "agent ${identity}: auth failed (${error_type})" \
      -m "Agent ${identity} cannot continue — error_type=${error_type}. Status set to offline. Requires human intervention (check API key, org membership, OAuth state)."
    ;;

  billing_error)
    "$st_bin" status "$identity" --set offline
    "$st_bin" message send operator \
      --priority high \
      --subject "agent ${identity}: billing issue" \
      -m "Agent ${identity} hit a billing_error and cannot continue. Status set to offline. Check billing status in the Anthropic console."
    ;;

  max_output_tokens | invalid_request | model_not_found)
    # Programmer error / long turn / config issue — not infrastructure.
    # The agent owns recovery from these; st stays out of it.
    :
    ;;

  *)
    # Anything not listed above — including the literal "unknown" value
    # Claude Code may emit, and any new error_type we haven't seen yet.
    # Status=away and ding with the type verbatim so we can extend the
    # table after triage.
    "$st_bin" status "$identity" --set away
    "$st_bin" message send operator \
      --subject "agent ${identity} wedged: ${error_type}" \
      -m "Agent ${identity} ended a turn on an unhandled error_type=${error_type}. Status set to away. Worth checking — this is an error_type the st StopFailure hook policy doesn't yet recognize."
    ;;
esac

exit 0
