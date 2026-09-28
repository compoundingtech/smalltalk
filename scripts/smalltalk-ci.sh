#!/usr/bin/env bash
set -euo pipefail

# Installed at /home/myobie/.local/share/smalltalk-ci/ci.sh on hetz. This is
# trusted fleet code; it is never read from a pull request checkout.
repo=compoundingtech/smalltalk
actor=agent/fleet/smalltalk/own-ci/2026-09-28/own-ci-builder
state=/home/myobie/.local/state/st3/smalltalk-ci
bin=${ST3_BIN:-/home/myobie/.local/bin/st3}
gh_bin=/usr/bin/gh
export GH_CONFIG_DIR=/home/myobie/.config/gh
git_bin=/usr/bin/git
cargo_bin=/home/myobie/.cargo/bin/cargo
socket=/home/myobie/.local/state/st3/st3.sock
mkdir -p "$state"

claim_json() {
  local source=$1 subject=${1%@*} claim=${1##*@}
  [[ $source == *@* && $claim =~ ^[a-f0-9]{64}$ ]] || { echo "source is not an exact resource claim" >&2; return 1; }
  curl --fail --silent --show-error --unix-socket "$socket" "http://localhost/v1/claims/by-id/$claim" |
    jq -e --arg subject "$subject" 'select(.value.subject == $subject and .value.kind == "resource.observed") | .value.body.fields.facts'
}

current_revision() {
  "$bin" subject show "mission/$1" --json | jq -er '.status.subjects[0].actual.revision'
}

same_repository_pull() {
  local number=$1
  "$gh_bin" api "repos/$repo/pulls/$number" |
    jq -e --arg repo "$repo" 'select(.head.repo.full_name == $repo and .base.repo.full_name == $repo and .draft == false)'
}

register_number() {
  local number=$1 mode=${2:-run} pull revision watch_file watch_id resource_id run_id
  [[ $number =~ ^[0-9]+$ ]] || { echo "invalid pull request number" >&2; return 1; }
  [[ $mode == run || $mode == skip ]] || { echo "invalid registration mode" >&2; return 1; }
  pull=$(same_repository_pull "$number") || { echo "PR $number is a fork or draft; no CI watcher"; return 0; }
  [[ $(jq -r '.state' <<<"$pull") == open ]] || { echo "PR $number is closed"; return 0; }
  mkdir -p "$state/watch/$number"
  if [[ $mode == skip ]]; then
    jq -er '.head.sha' <<<"$pull" > "$state/watch/$number/baseline-head"
  else
    rm -f "$state/watch/$number/baseline-head"
  fi
  revision=$(current_revision fleet/smalltalk/ci/run)
  watch_id="fleet/smalltalk/ci/watch/$number"
  resource_id="github/compoundingtech/smalltalk/ci/watch/$number"
  watch_file="$state/watch-$number.kdl"
  cat > "$watch_file" <<EOF
version 2

resource "$resource_id" { kind "vcs.ref" }
resource "fleet/smalltalk/ci/watch/$number/retirement" { kind "human.review" }

mission "$watch_id" state="ready" revision-cutover="restart-active" {
  goal "Watch same-repository pull request $number and run CI for each observed head."
  observer "pull" {
    resource "resource/$resource_id"
    provider "github.pull-request"
    locator "$repo#$number"
    field "head"
  }
  subscription "heads" {
    observer "observer/pull"
    on "head"
    delivery "mission" {
      mission "fleet/smalltalk/ci/run@$revision"
      resource "source"
      workspace "$state/runs"
      requester "$actor"
    }
  }
  step "initial" timeout="10m" {
    agentless
    gate "the initial head is queued" {
      exec "/home/myobie/.local/share/smalltalk-ci/ci.sh initial $number $mode"
      host "hetz"
      workspace "$state"
      time-limit "8m"
    }
  }
  step "keep-watch" {
    agentless
    gate "watcher retirement" {
      field "retired" "resource/fleet/smalltalk/ci/watch/$number/retirement" "is" "true"
    }
  }
}

# The standing repository observer emits an exact item claim on discovery, head changes,
# and closure. Start CI from that claim so no per-pull-request observer is needed.
register_source() {
  local source=$1 number facts head run_id pull
  [[ ${source%@*} =~ ^resource/github/compoundingtech/smalltalk/ci/pull-request/([0-9]+)$ ]] || {
    echo 'invalid discovery source' >&2
    return 1
  }
  number=${BASH_REMATCH[1]}
  facts=$(claim_json "$source")
  if [[ $(jq -r '.state' <<<"$facts") != open ]]; then
    retire_legacy_watcher "$number"
    return 0
  fi
  [[ $(jq -r '.draft' <<<"$facts") == false ]] || return 0
  pull=$(same_repository_pull "$number") || return 0
  head=$(jq -er '.head_sha' <<<"$facts")
  [[ $head =~ ^[a-f0-9]{40}$ && $head == "$(jq -r '.head.sha' <<<"$pull")" ]] || return 0
  run_id="fleet/smalltalk/ci/run/pr-$number-$head"
  "$bin" missions start fleet/smalltalk/ci/run --id "$run_id" --workspace "$state/runs" --input "source=$source" --as "$actor" ||
    "$bin" missions show "mission-run/$run_id" >/dev/null
  retire_legacy_watcher "$number"
}

retire_legacy_watcher() {
  local number=$1 run
  while IFS= read -r run; do
    if [[ $("$bin" missions show "$run" --json | jq -r '.status') == running ]]; then
      "$bin" missions cancel "$run" --as "$actor" --reason 'CI now uses the shared repository observer'
    fi
  done < <("$bin" missions ls --json |
    jq -r --arg id "mission/fleet/smalltalk/ci/watch/$number" \
      '.value.items[] | select(.id == $id) | .runs[]?')
}

retire_legacy_watchers() {
  local number
  while IFS= read -r number; do
    retire_legacy_watcher "$number"
  done < <("$bin" missions ls --json |
    jq -r '.value.items[] | .id | capture("^mission/fleet/smalltalk/ci/watch/(?<number>[0-9]+)$")? | .number' |
    sort -nu)
}
EOF
  "$bin" missions publish "$watch_file" --as "$actor"
  run_id=$watch_id
  local desired_revision existing_revision existing_status candidate
  desired_revision=$(current_revision "$watch_id")
  while IFS= read -r candidate; do
    if "$bin" missions show "$candidate" --json > "$state/watch/$number/candidate.json" 2>/dev/null &&
       [[ $(jq -r .status "$state/watch/$number/candidate.json") == running ]]; then
      run_id=${candidate#mission-run/}
      break
    fi
  done < <("$bin" missions ls --json |
    jq -r --arg id "mission/$watch_id" '.value.items[] | select(.id == $id) | .runs[]?')
  if "$bin" missions show "mission-run/$run_id" --json > "$state/watch/$number/existing.json" 2>/dev/null; then
    existing_revision=$(jq -er .revision "$state/watch/$number/existing.json")
    existing_status=$(jq -er .status "$state/watch/$number/existing.json")
    if [[ $existing_status == running && $existing_revision != "$desired_revision" ]]; then
      local revision_file="$state/watch/$number/revision.kdl"
      { printf 'version 2\n'; sed -n '/^mission "/,$p' "$watch_file"; } > "$revision_file"
      "$bin" work revise "mission-run/$run_id" "$revision_file" --as "$actor" --reason "Refresh the PR head watcher and pinned CI revision"
      return
    elif [[ $existing_status == running ]]; then
      return
    fi
    run_id="$watch_id/revision-${desired_revision:0:12}"
    if "$bin" missions show "mission-run/$run_id" --json > "$state/watch/$number/restarted.json" 2>/dev/null; then
      [[ $(jq -er .status "$state/watch/$number/restarted.json") == running ]] && return
      echo "watcher restart $run_id is terminal; needs repair" >&2
      return 1
    fi
  fi
  "$bin" missions start "$watch_id" --id "$run_id" --workspace "$state/watch/$number" --as "$actor"
}

initial() {
  local number=$1 mode=${2:-run} subject claim head run_id
  subject="resource/github/compoundingtech/smalltalk/ci/watch/$number"
  for _ in $(seq 1 60); do
    claim=$("$bin" subject history "$subject" --limit 100 --json 2>/dev/null |
      jq -rs '[.[] | select(.kind == "resource.observed" and .body.fields.facts.head != null)] | last | .id // empty')
    [[ -n $claim ]] && break
    sleep 2
  done
  [[ -n ${claim:-} ]] || { echo "PR $number observer did not establish a head" >&2; return 1; }
  [[ $mode == skip ]] && { echo "PR $number baseline recorded; future heads will run CI"; return 0; }
  head=$(claim_json "$subject@$claim" | jq -er .head)
  [[ $head =~ ^[a-f0-9]{40}$ ]] || return 1
  same_repository_pull "$number" >/dev/null || return 0
  run_id="fleet/smalltalk/ci/run/pr-$number-$head"
  "$bin" missions start fleet/smalltalk/ci/run --id "$run_id" --workspace "$state/runs" --input "source=$subject@$claim" --as "$actor" ||
    "$bin" missions show "mission-run/$run_id" >/dev/null
}

post_status() {
  local head=$1 value=$2 description=$3 run_id=$4 target
  target="https://github.com/compoundingtech/smalltalk/blob/main/docs/ci.md?run=$run_id"
  "$gh_bin" api --method POST "repos/$repo/statuses/$head" \
    -f "state=$value" -f 'context=st/ci' -f "description=$description" -f "target_url=$target" >/dev/null
}

# The commit GitHub has now for a pull request (NUMBER) or main (empty), or
# nothing when GitHub cannot be read, in which case the run goes ahead.
latest_head() {
  local number=$1 ref=refs/heads/main
  [[ -n $number ]] && ref=refs/pull/$number/head
  "$git_bin" ls-remote "https://github.com/$repo.git" "$ref" 2>/dev/null | awk 'NR == 1 { print $1 }'
}

# A run whose commit is no longer the head of its pull request or of main is
# skipped without a status: the newer head has its own run.
superseded() {
  local number=$1 head=$2 latest
  latest=$(latest_head "$number")
  [[ $latest =~ ^[a-f0-9]{40}$ && $latest != "$head" ]] || return 1
  echo "superseded by $latest"
}

skip_run() {
  local run_dir=$1 source=$2 head=$3 reason=$4
  mkdir -p "$run_dir"
  printf 'source=%s\nhead=%s\nskipped=%s\n' "$source" "$head" "$reason" > "$run_dir/summary"
  echo "skipped $head: $reason"
}

# Linux runs hold linux.lock, so any process still running from the shared
# target directory or this run's TMPDIR after the run is a leaked test daemon.
kill_leftovers() {
  local pattern
  for pattern in "$@"; do
    [[ -n $pattern ]] && pkill -9 -f -- "$pattern" 2>/dev/null || true
  done
}

linux() {
  local source=$1 facts number= head pull run_dir checkout is_main=false
  if [[ ${source%@*} =~ ^resource/github/compoundingtech/smalltalk/ci/(watch|pull-request)/([0-9]+)$ ]]; then
    number=${BASH_REMATCH[2]}
  elif [[ ${source%@*} == resource/github/compoundingtech/smalltalk/ci/main ]]; then
    is_main=true
  else
    echo "unexpected CI source" >&2; return 1
  fi
  facts=$(claim_json "$source")
  head=$(jq -er '(.head_sha // .head)' <<<"$facts")
  [[ $head =~ ^[a-f0-9]{40}$ ]] || return 1
  if [[ $is_main == false ]]; then
    pull=$(same_repository_pull "$number") || { echo "fork rejected before code execution" >&2; return 1; }
    if [[ $(jq -r .state <<<"$pull") != open ]]; then
      skip_run "$state/runs/$ST_MISSION_RUN" "$source" "$head" "pull request closed"
      return 0
    fi
    if [[ -f $state/watch/$number/baseline-head &&
          $(cat "$state/watch/$number/baseline-head") == "$head" ]]; then
      run_dir="$state/runs/$ST_MISSION_RUN"
      mkdir -p "$run_dir"
      printf 'source=%s\nhead=%s\nskipped=baseline\n' "$source" "$head" > "$run_dir/summary"
      echo "PR $number baseline head; waiting for the next head"
      return 0
    fi
  fi
  run_dir="$state/runs/$ST_MISSION_RUN"
  local reason
  if reason=$(superseded "$number" "$head"); then
    skip_run "$run_dir" "$source" "$head" "$reason"
    return 0
  fi
  ci_start=$(date +%s)
  checkout="$run_dir/repo"
  ci_head=$head
  ci_run_dir=$run_dir
  ci_status=1
  ci_skipped=
  # Each run gets its own short TMPDIR (Unix socket paths stay well under the
  # limit), removed with any test daemons it leaked when the run ends.
  ci_tmp=/tmp/ci$(printf %s "$ST_MISSION_RUN" | sha256sum | cut -c1-8)
  rm -rf "$ci_tmp"
  mkdir -p "$run_dir/logs" "$ci_tmp"
  printf 'source=%s\nhead=%s\nstarted=%s\n' "$source" "$head" "$(date -u +%FT%TZ)" > "$run_dir/summary"
  post_status "$head" pending "st3 CI running: $ST_MISSION_RUN" "$ST_MISSION_RUN"
  finish() {
    local elapsed=$(( $(date +%s) - ci_start )) result=failure
    kill_leftovers "${CARGO_TARGET_DIR:-$state/targets/linux}/" "$ci_tmp/"
    rm -rf "$ci_tmp" "$ci_run_dir/repo"
    if [[ -n $ci_skipped ]]; then
      printf 'skipped=%s\n' "$ci_skipped" >> "$ci_run_dir/summary"
      return
    fi
    [[ $ci_status == 0 ]] && result=success
    printf 'elapsed_seconds=%s\nresult=%s\n' "$elapsed" "$result" >> "$ci_run_dir/summary"
    post_status "$ci_head" "$result" "st3 CI $result in ${elapsed}s: $ST_MISSION_RUN" "$ST_MISSION_RUN" || true
  }
  trap finish EXIT
  if [[ ! -d $checkout/.git ]]; then
    "$git_bin" clone --quiet --no-checkout --reference /home/myobie/src/github.com/compoundingtech/smalltalk--main "https://github.com/$repo.git" "$checkout"
  fi
  if [[ $is_main == true ]]; then
    "$git_bin" -C "$checkout" fetch --quiet origin main
    "$git_bin" -C "$checkout" merge-base --is-ancestor "$head" origin/main || { echo "$head is not on main" >&2; return 1; }
  else
    "$git_bin" -C "$checkout" fetch --quiet origin main "refs/pull/$number/head:refs/remotes/origin/ci-head"
    if [[ $("$git_bin" -C "$checkout" rev-parse refs/remotes/origin/ci-head) != "$head" ]]; then
      ci_skipped="superseded while starting"
      post_status "$head" error "st3 CI superseded by a newer head: $ST_MISSION_RUN" "$ST_MISSION_RUN" || true
      return 0
    fi
  fi
  "$git_bin" -C "$checkout" checkout --quiet --detach "$head"
  if [[ $is_main == false ]]; then
    "$git_bin" -C "$checkout" -c user.name='Small Talk CI' -c user.email='ci@invalid.example' merge --quiet --no-edit origin/main
  fi
  cd "$checkout"
  mkdir -p "$run_dir/home/.config" "$run_dir/home/.local/share" "$run_dir/home/.cache" "$run_dir/home/.local/state" "${CI_CARGO_TARGET_DIR:-$state/targets/linux}"
  export HOME="$run_dir/home"
  export XDG_CONFIG_HOME="$HOME/.config" XDG_DATA_HOME="$HOME/.local/share"
  export XDG_CACHE_HOME="$HOME/.cache" XDG_STATE_HOME="$HOME/.local/state"
  export TMPDIR="$ci_tmp/"
  # The st service preloads jemalloc for itself; tests run without it.
  unset LD_PRELOAD MALLOC_ARENA_MAX
  export CARGO_HOME=/home/myobie/.cargo RUSTUP_HOME=/home/myobie/.rustup
  export CARGO_TARGET_DIR="${CI_CARGO_TARGET_DIR:-$state/targets/linux}" CARGO_BUILD_JOBS=8
  export RUST_TEST_THREADS=4
  export RUSTC_WRAPPER=/home/myobie/.local/bin/sccache-rustc
  local otelite_bins=(/nix/store/*otelite*/bin/otelite)
  local stdlibs=(/nix/store/*gcc*-lib/lib/libstdc++.so.6)
  if [[ -x ${otelite_bins[0]:-} && -e ${stdlibs[0]:-} ]]; then
    export ST2_OTELITE_BIN=${otelite_bins[0]}
    export LD_LIBRARY_PATH="${stdlibs[0]%/*}:${LD_LIBRARY_PATH:-}"
  else
    export ST2_ALLOW_OTEL_SKIP=1
  fi
  local wasm_tools=(/nix/store/*wasm-tools*/bin/wasm-tools)
  [[ -x ${wasm_tools[0]:-} ]] || { echo 'wasm-tools is required for provider component tests' >&2; return 1; }
  mkdir -p "$run_dir/components"
  "$cargo_bin" build --target wasm32-unknown-unknown --locked \
    -p st2-github-issue-component -p st2-github-pr-component \
    -p st2-pty-stats-component -p st2-vista-component > "$run_dir/logs/components.log" 2>&1
  local wasm_name
  for wasm_name in st2_github_issue_component st2_github_pr_component st2_pty_stats_component st2_vista_component; do
    "${wasm_tools[0]}" component new \
      "$CARGO_TARGET_DIR/wasm32-unknown-unknown/debug/$wasm_name.wasm" \
      -o "$run_dir/components/$wasm_name.component.wasm" >> "$run_dir/logs/components.log" 2>&1
  done
  export ST2_GITHUB_ISSUE_COMPONENT="$run_dir/components/st2_github_issue_component.component.wasm"
  export ST2_GITHUB_PR_COMPONENT="$run_dir/components/st2_github_pr_component.component.wasm"
  export ST2_PTY_STATS_COMPONENT="$run_dir/components/st2_pty_stats_component.component.wasm"
  export ST2_VISTA_COMPONENT="$run_dir/components/st2_vista_component.component.wasm"
  # A hung test fails the run at the time limit instead of holding linux.lock.
  /usr/bin/time -f 'elapsed_seconds=%e exit=%x' -o "$run_dir/logs/test.time" \
    /usr/bin/timeout --kill-after=30s 25m "$cargo_bin" test --workspace --locked > "$run_dir/logs/test.log" 2>&1
  /usr/bin/time -f 'elapsed_seconds=%e exit=%x' -o "$run_dir/logs/clippy.time" \
    /usr/bin/timeout --kill-after=30s 10m "$cargo_bin" clippy --workspace --all-targets --locked > "$run_dir/logs/clippy.log" 2>&1
  /usr/bin/time -f 'elapsed_seconds=%e exit=%x' -o "$run_dir/logs/codegen.time" \
    /usr/bin/timeout --kill-after=30s 10m "$cargo_bin" run -p st3-client-codegen --locked -- --check > "$run_dir/logs/codegen.log" 2>&1
  local baseline baseline_path
  baseline=$(/usr/bin/jq -er .commit .github/fleet-compat-baseline.json)
  [[ $baseline =~ ^[a-f0-9]{40}$ ]] || { echo 'invalid fleet compatibility baseline' >&2; return 1; }
  baseline_path="$state/baselines/$baseline"
  mkdir -p "$state/baselines"
  if [[ ! -x $baseline_path/bin/st3 ]]; then
    BASELINE="$baseline" /nix/var/nix/profiles/default/bin/nix build --impure \
      --out-link "$baseline_path" --expr '
        ((builtins.getFlake "github:compoundingtech/smalltalk/${builtins.getEnv "BASELINE"}")
          .packages.${builtins.currentSystem}.st3).overrideAttrs (_: { doCheck = false; })' \
      > "$run_dir/logs/fleet-baseline.log" 2>&1
  fi
  [[ -x $baseline_path/bin/st3 ]] || return 1
  if "$baseline_path/bin/st3" fleet --help > /dev/null 2>&1; then
    echo 'fleet compatibility baseline already supports fleet' >&2
    return 1
  fi
  ST3_COMPAT_BIN="$baseline_path/bin/st3" /usr/bin/time -f 'elapsed_seconds=%e exit=%x' \
    -o "$run_dir/logs/fleet-compat.time" /usr/bin/timeout --kill-after=30s 5m \
    "$cargo_bin" test -p st3 --test fleet --locked -- --ignored --exact \
    an_old_build_config_peer_replicates_with_new_members > "$run_dir/logs/fleet-compat.log" 2>&1
  /usr/bin/grep -q 'test result: ok\. 1 passed' "$run_dir/logs/fleet-compat.log"
  ci_status=0
  cat "$run_dir"/logs/*.time
}

finalize() {
  local source=$1 facts head linux_state result elapsed summary
  facts=$(claim_json "$source")
  head=$(jq -er '(.head_sha // .head)' <<<"$facts")
  summary="$state/runs/$ST_MISSION_RUN/summary"
  if [[ -f $summary ]] && grep -q '^skipped=' "$summary"; then
    echo "skipped run needs no commit status: $(sed -n 's/^skipped=//p' "$summary" | tail -1)"
    return 0
  fi
  linux_state=$("$bin" missions show "mission-run/$ST_MISSION_RUN" --json |
    jq -er '.steps[] | select(.step == "linux") | .status')
  result=failure
  [[ $linux_state == completed ]] && result=success
  elapsed=$(sed -n 's/^elapsed_seconds=//p' "$summary" 2>/dev/null | tail -1)
  [[ -n $elapsed ]] || elapsed=unknown
  post_status "$head" "$result" "st3 CI $result in ${elapsed}s: $ST_MISSION_RUN" "$ST_MISSION_RUN"
}

macos_remote() {
  local source=$1 number= head reason
  [[ $source == resource/github/compoundingtech/smalltalk/ci/*@[a-f0-9]* ]] || return 2
  head=$(claim_json "$source" | jq -er '(.head_sha // .head)')
  if [[ ${source%@*} =~ ^resource/github/compoundingtech/smalltalk/ci/(watch|pull-request)/([0-9]+)$ ]]; then
    number=${BASH_REMATCH[2]}
    if [[ -f $state/watch/$number/baseline-head &&
          $(cat "$state/watch/$number/baseline-head") == "$head" ]]; then
      echo "PR $number baseline head; no Silber check needed"
      return 0
    fi
    if [[ $("$gh_bin" api "repos/$repo/pulls/$number" --jq .state 2>/dev/null) == closed ]]; then
      echo "PR $number is closed; no Silber check needed"
      return 0
    fi
    # Nathan, 2026-09-28: pull requests merge on st/ci; macOS checks main only while Silber's
    # single lane takes about 15 minutes. A pull request labelled macos-ci still gets one.
    if [[ $("$gh_bin" api "repos/$repo/pulls/$number" --jq '[.labels[].name] | index("macos-ci") != null' 2>/dev/null) != true ]]; then
      echo "PR $number: macOS checks main only; label the pull request macos-ci for a Silber run"
      return 0
    fi
  fi
  if reason=$(superseded "$number" "$head"); then
    echo "no Silber check for $head: $reason"
    return 0
  fi
  # Silber has one macOS lane: spend it only on heads that pass on Linux.
  if [[ $("$bin" missions show "mission-run/$ST_MISSION_RUN" --json |
          jq -r '.steps[] | select(.step == "linux") | .status') != completed ]]; then
    echo "no Silber check for $head: the Linux check did not pass"
    return 0
  fi
  /home/myobie/.local/bin/fabric exec silber -- /usr/bin/lockf -t 4200 \
    /Users/myobie/.local/state/st3/smalltalk-ci/macos.lock \
    /Users/myobie/.local/share/smalltalk-ci/ci-macos.sh "$source" "$ST_MISSION_RUN"
}

case ${1:-} in
  register) register_source "${2:?exact discovery source}" ;;
  retire-legacy-watchers) retire_legacy_watchers ;;
  register-number) register_number "${2:?pull number}" "${3:-run}" ;;
  initial) initial "${2:?pull number}" "${3:-run}" ;;
  linux) linux "${2:?exact source}" ;;
  macos-remote) macos_remote "${2:?exact source}" ;;
  finalize) finalize "${2:?exact source}" ;;
  *) echo 'usage: ci.sh {register|register-number|initial|linux|finalize} ARG' >&2; exit 2 ;;
esac
