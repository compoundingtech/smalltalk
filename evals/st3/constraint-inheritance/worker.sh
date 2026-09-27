#!/usr/bin/env bash
set -euo pipefail

outer="step-run/${ST_RUN_GENERATION}/outer"
claim=""
for _ in $(seq 1 300); do
  if claim=$(st3 --json work claim "$outer" 2>/dev/null); then
    break
  fi
  sleep 0.2
done
test -n "$claim"

printf '%s\n' "$claim" | jq -e '.constraints == [
  "Keep the mission result local.",
  "Keep the outer result deterministic."
]' >/dev/null

st3 work complete "$outer" --summary "The claim contained both inherited constraints in order." >/dev/null

leaf="step-run/${ST_RUN_GENERATION}/outer/work/leaf"
claim=""
for _ in $(seq 1 300); do
  if claim=$(st3 --json work claim "$leaf" 2>/dev/null); then
    break
  fi
  sleep 0.2
done
test -n "$claim"

printf '%s\n' "$claim" | jq -e '.constraints == [
  "Keep the mission result local.",
  "Keep the outer result deterministic.",
  "Keep the nested mission result readable.",
  "Keep the leaf result complete."
]' >/dev/null

: >constraint-inheritance.ok
st3 work complete "$leaf" --summary "The claim contained all four inherited constraints in order." >/dev/null
