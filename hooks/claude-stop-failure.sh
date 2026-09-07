#!/usr/bin/env bash
# st2 Claude StopFailure hook: surface infrastructure wedges through presence and, when declared,
# the agent's supervisor inbox. Fail-open; lifecycle reporting must never wedge the harness.

set -u

identity="${ST_AGENT:-}"
root="${ST_ROOT:-${CATALOG:-}}"
supervisor="${ST_SUPERVISOR:-}"
if [[ -z "$identity" || -z "$root" ]] || ! command -v st2 >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; then
  exit 0
fi

input="$(cat)"
# Claude spells the closed error word `error` on the StopFailure payload (`error_type` is not a
# field it ever emits), and the sibling observe hook reads the same field.
error_type="$(printf '%s' "$input" | jq -r '.error // "unknown"' 2>/dev/null || printf unknown)"
status="away"
notify="yes"
case "$error_type" in
  rate_limit)
    notify=""
    ;;
  authentication_failed | oauth_org_not_allowed | billing_error)
    status="offline"
    ;;
  max_output_tokens | invalid_request | model_not_found)
    exit 0
    ;;
esac

st2 status "$identity" --root "$root" --set "$status" >/dev/null 2>&1 || true
if [[ -n "$notify" && -n "$supervisor" ]]; then
  st2 message send "$supervisor" \
    --root "$root" \
    --as "$identity" \
    --subject "agent ${identity} stopped: ${error_type}" \
    -m "Agent ${identity} ended a Claude turn with error_type=${error_type}. Status set to ${status}; inspect or nudge it when appropriate." \
    >/dev/null 2>&1 || true
fi
exit 0
