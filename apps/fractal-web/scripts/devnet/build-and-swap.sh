#!/usr/bin/env bash
# Build/activate only. The integration-loop owner manages the new devnet purpose.
set -Eeuo pipefail
ROOT=${FRACTAL_BUILD_ROOT:?missing required input: FRACTAL_BUILD_ROOT}
SNAPSHOT_ROOT=${FRACTAL_BUILD_SNAPSHOT_ROOT:?missing required input: FRACTAL_BUILD_SNAPSHOT_ROOT}
DEPCACHE=${FRACTAL_DEP_CACHE:?missing required input: FRACTAL_DEP_CACHE}
script_dir=$(dirname -- "$(realpath -- "${BASH_SOURCE[0]}")")
SOURCE=${1:?Usage: build-and-swap.sh SOURCE_DIR SHA NAME}
requested=${2:?Usage: build-and-swap.sh SOURCE_DIR SHA NAME}
name=${3:?Usage: build-and-swap.sh SOURCE_DIR SHA NAME}
[[ $name =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]*$ && $name != . && $name != .. ]] || { echo 'Invalid deployment name' >&2; exit 2; }
SOURCE=$(realpath "$SOURCE")
# Validate the security boundary before creating build roots or worktrees.
python3 "$script_dir/candidate_sandbox.py" --check-config
# A sandbox-policy namespace cannot reuse an immutable pre-sandbox layout.
dependency_policy=$(python3 "$script_dir/candidate_sandbox.py" --print-policy)
DEPCACHE=$DEPCACHE/$dependency_policy
mkdir -p "$ROOT" "$SNAPSHOT_ROOT" "$DEPCACHE"
ROOT=$(realpath -- "$ROOT")
SNAPSHOT_ROOT=$(realpath -- "$SNAPSHOT_ROOT")
DEPCACHE=$(realpath -- "$DEPCACHE")
case "$DEPCACHE/" in
  "$SOURCE/"*) echo 'Dependency cache must be outside the mutable source clone' >&2; exit 2 ;;
esac
P=$ROOT/$name
started=$(date +%s%3N)
sha=$requested
outcome=failed
reason=unexpected-error
snapshot=
snapshot_lock=
build=
previous=
activated=0
cleanup_warnings=
mkdir -p "$P/builds"
log_run() {
  TS=$(date -u +%FT%TZ) SHA=$sha OUTCOME=$outcome REASON=$reason MS=$(($(date +%s%3N)-started)) CLEANUP_WARNINGS=$cleanup_warnings python3 -c 'import json,os; print(json.dumps({"ts":os.environ["TS"],"sha":os.environ["SHA"],"outcome":os.environ["OUTCOME"],"reason":os.environ["REASON"],"ms":int(os.environ["MS"]),"cleanupWarnings":os.environ["CLEANUP_WARNINGS"].splitlines()}))' >> "$P/builds.jsonl"
}
cleanup_warning() {
  printf 'WARNING: cleanup: %s\n' "$1" >&2
  cleanup_warnings+="$1"$'\n'
}
finish() {
  local rc=$?
  local rollback_failed=0 selected=0 current_target='' recorded_sha=''
  trap - EXIT
  set +e
  # Command wrappers may need getcwd even with absolute arguments. Leave the
  # snapshot before deleting it, including on failed/partial builds.
  if ! cd -- "$ROOT" && ! cd /; then
    printf 'ERROR: cleanup cannot enter a stable working directory\n' >&2
    exit 1
  fi
  if [[ $outcome == failed ]]; then
    echo "$(date -u +%FT%TZ) failed sha=$sha reason=$reason exit=$rc" >&2
    if ((activated)); then
      if [[ -n $previous ]]; then
        if ! ln -s "$previous" "$P/.current-rollback-$$" ||
            ! mv -Tf "$P/.current-rollback-$$" "$P/current"; then
          cleanup_warning 'unsafe rollback failure: current may reference a failed build'
          rollback_failed=1
          reason=rollback-failed
          rc=1
        fi
      elif ! rm -f "$P/current"; then
        cleanup_warning 'unsafe rollback failure: cannot remove failed current pointer'
        rollback_failed=1
        reason=rollback-failed
        rc=1
      fi
    fi
    if [[ -n $build ]]; then
      # Pointer rollback and artifact deletion are separate decisions. Failed
      # activation must never delete an artifact still named by either pointer.
      if [[ -e $P/current || -L $P/current ]]; then
        if ! current_target=$(readlink -f -- "$P/current"); then
          selected=1
          cleanup_warning 'cannot resolve current; preserving failed build'
        elif [[ $current_target == "$build" ]]; then selected=1; fi
      fi
      if [[ -e $P/current-sha || -L $P/current-sha ]]; then
        if ! IFS= read -r recorded_sha < "$P/current-sha" ||
            [[ ! $recorded_sha =~ ^[0-9a-f]{40}$ || $recorded_sha == "${build##*/}" ]]; then
          selected=1
        fi
      fi
      if ((rollback_failed || selected)); then
        cleanup_warning 'unsafe activation state: preserving failed build because rollback or selection is inconsistent'
        ((rollback_failed)) || reason=rollback-inconsistent
        rc=1
      else
        rm -rf -- "$build" || cleanup_warning 'cannot remove failed build directory'
      fi
    fi
  fi
  rm -f "$P/admitted-$$" "$P/overlay-manifest-$$" || cleanup_warning 'cannot remove temporary admission/overlay files'
  if [[ -n $snapshot ]]; then
    git -C "$SOURCE" worktree remove --force "$snapshot" || cleanup_warning 'cannot remove snapshot worktree; later stale reclamation can retry'
  fi
  [[ -z $snapshot_lock ]] || rm -f -- "$snapshot_lock" || cleanup_warning 'cannot remove snapshot lock file'
  log_run || cleanup_warning 'cannot append build log'
  # Temporary-file/worktree/log cleanup is recoverable and must not undo a
  # successful build status. Unsafe rollback above remains a hard failure.
  if [[ $outcome == failed && $rc == 0 ]]; then rc=1; fi
  exit "$rc"
}
trap finish EXIT
exec 9>"$P/build.lock"
if ! flock -n 9; then outcome=skipped; reason=lock-held; exit 0; fi
reason=revision-unavailable
sha=$(git -C "$SOURCE" rev-parse --verify "$requested^{commit}")
if [[ -f $P/current-sha && $(<"$P/current-sha") == "$sha" && -f $P/current/build-receipt.json ]]; then outcome=skipped; reason=already-current; exit 0; fi
previous=$(readlink -f "$P/current" || true)
[[ -d $previous ]] || previous=
# Snapshots stay in the repository store's worktrees tree; the dependency cache
# shares their filesystem, independently of the mutable integration clone.
snapshot_root=$SNAPSHOT_ROOT/$name
mkdir -p "$snapshot_root"
snapshot_root=$(realpath -- "$snapshot_root")
case "$snapshot_root/" in
  "$SOURCE/"*) reason=recursive-snapshot-root; echo 'Snapshot root must be outside the source clone' >&2; exit 2 ;;
esac
if [[ $(stat -c %d "$DEPCACHE") != $(stat -c %d "$snapshot_root") ]]; then
  reason=cross-device-dependency-cache
  echo 'Snapshot root must share the immutable dependency cache filesystem' >&2
  exit 2
fi
# Snapshot leases are keyed by the shared namespace, not by the build root.
for stale in "$snapshot_root"/*; do
  [[ -d $stale && ! -L $stale && ${stale##*/} =~ ^[0-9a-f]{40}-[0-9]+$ ]] || continue
  (
    exec 7>"$stale.lock"
    if flock -n 7; then
      git -C "$SOURCE" worktree remove --force "$stale" || rm -rf -- "$stale"
      rm -f -- "$stale.lock"
    fi
  )
done
next_snapshot=$snapshot_root/$sha-$$
exec 8>"$next_snapshot.lock"
if ! flock -n 8; then outcome=skipped; reason='snapshot-lock-held'; exit 0; fi
snapshot=$next_snapshot
snapshot_lock=$snapshot.lock
build=$P/builds/$sha
if [[ -e $build ]]; then build=; reason=immutable-build-already-exists; exit 1; fi
reason=dependency-cache-failed
exec 6>"$DEPCACHE/.cache.lock"
flock -x 6
dependency_key=$(python3 "$script_dir/dependency_cache.py" key "$SOURCE")
python3 "$script_dir/dependency_cache.py" populate "$SOURCE" "$DEPCACHE" "$dependency_key"
generation=$DEPCACHE/$dependency_key
# Acquire the shared generation lease before releasing the publication/prune
# lock. FD 5 stays open until after finish() removes this run's snapshot.
exec 5<"$generation/.lease"
flock -s 5
flock -u 6
exec 6>&-
reason='snapshot-failed'
git -C "$SOURCE" worktree add --detach "$snapshot" "$sha"
python3 "$script_dir/prepare-dependencies.py" "$SOURCE" "$snapshot" "$P/overlay-manifest-$$" "$generation" "$dependency_key"
cd "$snapshot/apps/fractal-web"
identity=$snapshot/apps/fractal-web/vendor/utils/src/node/cli-build-identity.js
# Admission and activation stay on the host; the complete private builder and
# its children run in the networkless sandbox, including resource fallbacks.
sandbox_command=(python3 "$script_dir/candidate_sandbox.py"
  --worktree "$snapshot" --shell web --output "$build" --readonly-dependencies
  --directory "$snapshot/apps/fractal-web"
  -- node private/build-live.mjs "$build" "$identity" "$sha")
reason='build-failed'
# A bisect caller may reuse this tick's observed admission timeout for later candidates.
if [[ ${FRACTAL_BUILD_ADMISSION:-gate} == fallback ]]; then
  echo 'Caller reuses this tick admission timeout; building with plain nice -n 19' >&2
  nice -n 19 "${sandbox_command[@]}"
  mode=caller-fallback-nice-19
elif [[ ${FRACTAL_BUILD_ADMISSION:-gate} != gate ]]; then
  reason=invalid-admission-mode
  exit 2
elif gate-slot --class gate --max-wait 600 -- nice -n 10 bash -c "touch \"\$1\"; shift; exec \"\$@\"" -- "$P/admitted-$$" "${sandbox_command[@]}"; then
  mode=gate-nice-10
else
  status=$?
  if [[ ! -e $P/admitted-$$ && $status != 75 ]]; then
    echo 'Gate admission unavailable within 10 minutes; building with plain nice -n 19' >&2
    nice -n 19 "${sandbox_command[@]}"
    mode=fallback-nice-19
  else exit "$status"; fi
fi
cp "$P/overlay-manifest-$$" "$build/overlay-snapshot.json"
reason=receipt-invalid
python3 - "$build/build-receipt.json" "$sha" "$build/overlay-snapshot.json" "$script_dir" "$snapshot" <<'PY'
import json,sys
sys.path.insert(0, sys.argv[4])
import candidate_sandbox
r=json.load(open(sys.argv[1])); assert r['buildIdentity']['rev']==sys.argv[2] and r['buildIdentity']['dirty'] is False and r['stackHead']==sys.argv[2]
manifest=json.load(open(sys.argv[3]))
r.update(dependencyKey=manifest['dependencyKey'],dependencyFiles=manifest['dependencyFiles'])
r['devShell'] = candidate_sandbox.shell_provenance(sys.argv[5])
with open(sys.argv[1],'w') as f: f.write(json.dumps(r,indent=2)+'\n')
PY
chmod +x "$build/launch.sh"
reason=bundle-invalid
python3 - "$build/web/index.html" <<'PY'
import re,sys
s=open(sys.argv[1]).read(); assert '/@vite/client' not in s and re.search(r'/assets/[^"\s]+\.js',s)
PY
reason=activation-failed
ln -s "$build" "$P/.current-$$"
mv -Tf "$P/.current-$$" "$P/current"
activated=1
printf '%s\n' "$sha" > "$P/.current-sha-$$"
mv -f "$P/.current-sha-$$" "$P/current-sha"
outcome=built
reason=$mode
# The integrator prunes only after the published edge confirms activation.
printf 'BUILD_DIR=%s\nCURRENT=%s\nSHA=%s\n' "$build" "$P/current" "$sha"
