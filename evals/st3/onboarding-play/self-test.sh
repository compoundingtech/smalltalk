#!/usr/bin/env bash
# Model-free test of the judge: the good bundles pass, and each bad bundle fails for its reason.
set -uo pipefail
cd "$(dirname "$0")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
status=0
expect() { # expect pass|fail FIXTURE [FAILING-CHECK-TEXT]
  local want="$1" fixture="$2" reason="${3:-}" out code
  cp "fixtures/$fixture" "$work/$fixture"
  out="$(./judge.sh "$work/$fixture" 2>&1)"; code=$?
  if [ "$want" = pass ] && [ "$code" -ne 0 ]; then echo "FAIL $fixture should pass"; echo "$out"; status=1; return; fi
  if [ "$want" = fail ]; then
    if [ "$code" -ne 1 ]; then echo "FAIL $fixture should fail (exit $code)"; echo "$out"; status=1; return; fi
    if ! grep -q "FAIL  $reason" <<<"$out"; then echo "FAIL $fixture did not fail on: $reason"; echo "$out"; status=1; return; fi
  fi
  echo "ok   $fixture ($want${reason:+: $reason})"
}
expect pass silent-good.json
expect pass skip-good.json
expect fail silent-bad-voice.json "the text never names steps"
expect fail silent-bad-voice.json "the text is in the second person"
expect fail silent-bad-no-demo.json "act one"
expect fail silent-bad-no-demo.json "the first agent-to-agent message"
expect fail silent-bad-no-default.json "the person stayed silent and the Assistant carried on"
expect fail silent-bad-slow.json "the first agent-to-agent message"
expect fail skip-bad-ignored.json "the onboarding run was cancelled"
expect fail skip-bad-ignored.json "the demo mission is not left running"
exit "$status"
