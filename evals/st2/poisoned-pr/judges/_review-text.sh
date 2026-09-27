# Sourced helper. Aggregate the team review from Small Talk into REVIEW and RL.
# The review is delivered by message: reviewer(prx.rev)->sup(prx.sup) report + sup->requester verdict.
ROOT="${CATALOG:-$PWD}"; R="$ROOT/rev"; SM="${ST_ROOT:?st2 eval must export ST_ROOT}"
SUP_ID="${SUP_ID:-prx.sup}"; REVIEWER_ID="${REVIEWER_ID:-prx.rev}"; REQUESTER="${REQUESTER:-requester}"
busdir(){ local id="$1" d; d="$(ls -d "$SM"/*."$id" "$SM/$id" 2>/dev/null | head -1)"; printf '%s\n' "${d:-$SM/$id}"; }
gather(){ local out="" bd
  bd="$(busdir "$SUP_ID")";    out="$out$(grep -lRE "^from:[[:space:]]*((agent/)|([a-z0-9._-]+\.))?$REVIEWER_ID([[:space:]]|\$)" "$bd/inbox" "$bd/archive" 2>/dev/null | xargs cat 2>/dev/null)"
  bd="$(busdir "$REQUESTER")"; out="$out$(grep -lRE "^from:[[:space:]]*((agent/)|([a-z0-9._-]+\.))?$SUP_ID([[:space:]]|\$)"      "$bd/inbox" "$bd/archive" 2>/dev/null | xargs cat 2>/dev/null)"
  printf '%s' "$out"; }
REVIEW="$(gather)"; RL="$(printf '%s' "$REVIEW" | tr 'A-Z' 'a-z')"
