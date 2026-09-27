# Sourced by the controller and judges. `WAKE_PAIRS` lists `left:right` participant pairs; each
# participant is the agent `wake.<name>`. The default is the committed Codex/Claude and Pi/OMP
# lanes. A variant can declare different seats without changing the protocol or its gates.

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the eval mission run}"

read -r -a wake_pairs <<<"${WAKE_PAIRS:-codex:claude pi:omp}"
wake_token_pool=(EMBER ORBIT QUARTZ RIVER)

if [ "${#wake_pairs[@]}" -lt 1 ] || [ "${#wake_pairs[@]}" -gt 2 ]; then
  printf 'WAKE_PAIRS must name one or two participant pairs\n' >&2
  exit 2
fi

names=()
declare -A agents peers tokens results
wake_index=0
for wake_pair in "${wake_pairs[@]}"; do
  wake_left="${wake_pair%%:*}"
  wake_right="${wake_pair#*:}"
  names+=("$wake_left" "$wake_right")
  agents[$wake_left]="agent/$ST_MISSION_RUN/wake.$wake_left"
  agents[$wake_right]="agent/$ST_MISSION_RUN/wake.$wake_right"
  peers[$wake_left]="${agents[$wake_right]}"
  peers[$wake_right]="${agents[$wake_left]}"
  # The pool is in ascending ASCII order, so the left token always sorts first.
  tokens[$wake_left]="${wake_token_pool[wake_index]}"
  tokens[$wake_right]="${wake_token_pool[wake_index + 1]}"
  results[$wake_left]="${tokens[$wake_left]}+${tokens[$wake_right]}"
  results[$wake_right]="${results[$wake_left]}"
  wake_index=$((wake_index + 2))
done
