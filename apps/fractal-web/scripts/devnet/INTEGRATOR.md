# Web integration and main follower

`integrator.py` selects Fractal Web and Fractal UI pull requests from
`compoundingtech/fractal`, integrates them in a disposable clone, materializes
locked megarepo members, and publishes an immutable build through Devnet.
`follow.sh` fast-forwards a separate main clone every five minutes, materializes
members with the effect-utils revision from `flake.lock`, and installs changed
workspace dependencies in the web shell. Both scripts write JSONL operational
logs. The follower includes failed install output in its log.

The integrator resolves web/genie dev shells **only from its trusted, pinned main
launcher checkout**, never from the candidate's flake. Candidate shell definitions
and flake inputs are not evaluated on the host. All hooks, generators, installs,
and application builds run inside bubblewrap. A candidate that needs a new
toolchain builds with the old pinned main toolchain until that change is merged
and the launcher pin is updated. `build-receipt.json` records `devShell.source`,
SHA256 pairs for `flake.nix` and `flake.lock`, `candidateDiffers`, and an explicit
toolchain-mismatch note. This divergence is informational, not a fallback to
candidate evaluation.
The trusted hook publishes `.devenv/pnpm-source-inputs` before
`pnpm install --prefer-offline --frozen-lockfile`. The generated-metadata cache
includes `pnpm-source-inputs.json`; the sandbox policy version invalidates old
dependency caches when adopting this boundary.

## Integrator inputs

Variables below are required unless marked conditional. Paths identify
deployment-owned resources; scripts do not provision deployments or credentials.

| Environment variable | Value |
| --- | --- |
| `FRACTAL_INTEGRATOR_STATE_ROOT` | Existing state root; logs and caches remain under `live/` and recorded stack clones under `clones/` |
| `FRACTAL_INTEGRATOR_CLONE` | Disposable integration clone; the script resets this clone |
| `FRACTAL_INTEGRATOR_PROD_ROOT` | Immutable production builds, including the `current` symlink |
| `FRACTAL_INTEGRATOR_PURPOSE` | Devnet purpose for the production service |
| `FRACTAL_INTEGRATOR_SOURCE_CLONE` | Local source clone used when initializing the integration clone |
| `FRACTAL_INTEGRATOR_PRIVATE_OVERLAY` | Deployment-owned private web overlay directory |
| `FRACTAL_INTEGRATOR_VENDOR` | Deployment-owned web vendor directory |
| `FRACTAL_INTEGRATOR_BUILD_SCRIPT` | Existing build-and-swap shell script; receives clone, revision and build name |
| `FRACTAL_INTEGRATOR_PRIORITY_JSON` | Ordered JSON array of required PR numbers; `[]` explicitly selects none |
| `FRACTAL_INTEGRATOR_TRUST_MODE` | Required: `authors` or `collaborator-permission`; no default |
| `FRACTAL_INTEGRATOR_TRUSTED_AUTHORS` | Conditional: explicit JSON array of trusted GitHub logins, required in `authors` mode; no built-in authors |
| `FRACTAL_INTEGRATOR_SANDBOX` | Required: `bwrap`; no default or unsandboxed fallback |
| `FRACTAL_INTEGRATOR_SANDBOX_STATE_ROOT` | Absolute, deployment-owned bootstrap scratch directory outside candidate clones and snapshots |
| `FRACTAL_INTEGRATOR_PNPM_STORE` | Absolute, dedicated writable pnpm package cache outside candidate clones; must contain no credentials |

Run `python3 apps/fractal-web/scripts/devnet/integrator.py` for the five-minute
loop, or append `--once` for one tick. `--dry-run` additionally requires
`FRACTAL_DRY_RUN_CLONE`; it merges and regenerates dependencies without building
or swapping production. A dry run still mutates its disposable clone and caches.

Existing state names remain unchanged: `live-r3.jsonl`, `live-r3-drop.json`,
`live-r3.lock`, `live-r3-required-stack.json`, pin/state/tree caches and
`<clone-name>-members-key.json`. Keep the integration clone basename unchanged
when adopting the script to reuse the existing member key. The production build
helper and generated launchers remain deployment-owned inputs, not replacements
for the separate launcher scripts.

## Trust boundary

Only same-repository PR heads are eligible: `isCrossRepository` must be false
and the head owner must match the canonical `compoundingtech/fractal` owner.
Fork heads are never eligible. Deployment owners must choose a trust mode:
`FRACTAL_INTEGRATOR_TRUST_MODE=authors` requires author membership in
`FRACTAL_INTEGRATOR_TRUSTED_AUTHORS`; `[]` explicitly trusts no authors.
`FRACTAL_INTEGRATOR_TRUST_MODE=collaborator-permission` instead requires GitHub
repository `write`, `maintain` or `admin` permission, queried once per author per
tick. API failures and read-only permissions are untrusted. This mode does not
require an author allowlist. Missing or unknown trust modes fail before a tick.
Rejected PRs log `fork-head` or `untrusted-author`, including required PRs.
After fetching, a moved head logs `head-moved`; merges use only the exact selected
and verified SHA. Recorded semantic stacks cannot restore rejected or moved heads.

## Publication recovery and retention

Cached successful artifacts under `<prod-root>/builds/<sha>` remain publishable
after an activation failure: the integrator reselects the artifact and retries
publication without rebuilding. An unchanged candidate is not considered served
until the published edge reports that exact revision. Reduced-stack cache entries
record the SHA that actually built successfully, including the ejected PR result.
After confirmed activation, the integrator keeps the current build and one previous
rollback build and deletes older build directories. `integrator-activation.json`
under the production root preserves the confirmed current/previous pair across
unchanged ticks and restarts. Failed activation does not prune builds.
The deployment-owned build helper must leave build cleanup to the integrator;
it must not prune artifacts before publication or remove the rollback target.

## Build helper inputs and snapshot I/O

Set `FRACTAL_INTEGRATOR_BUILD_SCRIPT` to this directory's `build-and-swap.sh`.
The helper takes `SOURCE_DIR SHA NAME` arguments and requires:

| Environment variable | Value |
| --- | --- |
| `FRACTAL_BUILD_ROOT` | Deployment-owned parent of per-name production directories (`<root>/<name>/builds/<sha>`) |
| `FRACTAL_BUILD_SNAPSHOT_ROOT` | Deployment-owned snapshot parent under the repository store's `worktrees/` directory, outside the source clone |
| `FRACTAL_DEP_CACHE` | Dedicated immutable dependency cache outside the mutable source clone, on the snapshot filesystem |

All three root inputs are canonicalized against the caller's directory before the helper
changes into a snapshot, so relative roots retain their original meaning.

The helper creates detached worktrees at `<snapshot-root>/<name>/<sha>-<pid>`.
Each run holds an exclusive `<snapshot>.lock` flock for its snapshot's lifetime.
Reclamation removes a matching revision/PID snapshot only after acquiring that
snapshot's lock non-blocking, so runs with different build roots can safely share
the snapshot namespace. Host Git remains the deployment's PATH Git, including its
worktree-location policy. The source clone need not share the snapshot filesystem;
the immutable dependency cache must share it.

`dependency_cache.py` first computes the original public digest by hashing each
relative path followed by its bytes, in this exact order: `pnpm-lock.yaml`,
`pnpm-workspace.yaml`, `megarepo.kdl`, `megarepo.lock`, `pnpm-source-inputs.json`.
The generation key is `sha256("fractal-dependency-key-v3\0" + public_hex +
"\0" + private_hex + "\0" + installed_hex)` (UTF-8 text, literal NUL separators).
The public five-file digest remains the first component. Public/private source
digests provide early invalidation; the **installed-tree stat fingerprint is the
authoritative ordinary-staleness guard**, including installs configured through
workspace overrides or other input formats not traversed by manifest inspection.
The private digest conservatively hashes **every source file** beneath
`apps/fractal-web/private` and `apps/fractal-web/vendor`, not a filename allowlist.
Arbitrary lifecycle scripts can read non-manifest files, so a minimal static
install closure is not reliable. Even unrelated private source changes invalidate
the cache. Directory exclusions are only `node_modules` (installed output, which
must not recursively key itself) and `.git` (VCS bookkeeping, not package source).
Build/dist/cache outputs are included: no evidence establishes a safe exclusion.
If the build writes such paths inside an overlay, that change invalidates the next
key. Adding a future output exclusion requires proving/asserting that no generation
inventory entry can capture dependencies beneath the excluded path.
Package manifests in these trees recursively add all source files of their
`file:`, `link:` and `workspace:` dependencies, including packages outside the
overlay but inside the source clone. Workspace names must resolve to exactly one
source package manifest. Missing/ambiguous local packages, malformed manifests,
and inputs escaping the source clone fail closed. Source symlinks contribute their
literal target plus in-clone target contents; overlay roots cannot be symlinks.
All collected entries are sorted by source-clone-relative path. Each contributes
`file`/`link` bytes, one byte of executable mode bits (zero for links), an 8-byte
big-endian path length, UTF-8 relative path, 8-byte big-endian content length, then
exact content/target bytes. Empty overlays produce SHA256 of empty bytes. Private
source contents are never committed; only the derived key enters deployment state.

The installed fingerprint inventories exactly the non-overlapping `node_modules`
trees that population copies, using one shared root-selection function. It hashes
compact JSON (`separators=(",", ":")`, default ASCII escaping) of a path-sorted list
of tuples: source-relative path, `S_IFMT(mode)`, size, `mtime_ns`, `S_IMODE(mode)`,
and literal symlink target (null for non-links), including root directories.
It performs stat/lstat/readlink only, not payload reads, and excludes inode/nlink
so incidental source hardlinks do not invalidate it. Unchanged installed metadata
keeps the key; ordinary reinstallation changing bytes changes mtimes/sizes and
therefore the key. This is not a content proof against hostile same-UID edits that
restore all metadata; the sandbox boundary remains required for hostile code.
Population checks the source key before copying, after copying, and after sealing/
prune waits immediately before publication. Drift discards staging rather than
publishing a generation selected from a stale fingerprint.
The sandbox helper uses `<FRACTAL_DEP_CACHE>/bwrap-v1` as this effective cache root.
`candidate_sandbox.py --print-policy` exposes the shared policy namespace without
requiring deployment configuration. Generation basenames retain the parent's v3
public/private/installed-tree key; the child-only namespace prevents pre-sandbox
layout reuse.
Under an exclusive `<cache>/.cache.lock`, initial population copies installed
dependency trees from the mutable clone into a staging directory and atomically
renames it into place. Source-to-cache copying never hardlinks mutable source files.
Publication removes all write bits from regular files and directories, records
per-file integrity metadata and hashes in schema-1 `.files.json`, and fully
verifies SHA256 before selection.
Clean reuse checks file inventory, size, mode, mtime_ns, inode identity and link-count
sanity without rereading all dependency contents. A metadata mismatch triggers a
full diagnostic rehash, exclusive-lease retirement, and source-based rebuild, never
in-place repair. Snapshot directories are independently writable; dependency file
inodes remain read-only. These mode bits stop accidental writes, **not hostile code
running as the same UID**; the sandbox PR supplies the hostile-code boundary.
`.fractal-dep-key` and `.node-modules.json` identify the key and safe source-relative
dependency directories; mismatched markers or escaping inventories fail closed.
Workspace symlinks are normalized in staging to clone-independent relative topology:
an absolute link into clone A's `packages/` becomes a relative workspace link, so
the same-key generation reused by clone B cannot retain A's mutable path.
Published generations with incompatible integrity metadata or absolute/escaping
links require a fresh cache root. The v3 key intentionally does not select earlier
key formats; no generation is migrated in place.
These directories are disposable copies of the clone's installed dependencies;
a fresh population rebuilds their contents from that source, not application data.

`.selection.json` outside the generations records the selected current and previous
generation. Reusing an older generation updates selection lineage without mutating it.
The helper takes a shared generation `.lease` before releasing the cache lock and
holds that lease through preparation, build and snapshot cleanup. Before publishing
a new generation, pruning holds the cache lock and waits for an exclusive lease on
each obsolete generation; running builds release leases without reacquiring the
cache lock. This preserves current plus the previously selected generation and never
deletes one in use. A new dependency selection can therefore wait for an older build.
Under that exclusive lease, pruning removes selection references and atomically
renames the obsolete directory into `<cache>/.garbage/` before recursive deletion.
The exclusive owner temporarily enables writing on the generation root for Linux
cross-parent rename; no active snapshot can still hold its lease. Only retired
garbage directories are subsequently made writable for deletion; dependency file
modes are never changed during cleanup.
Garbage is never a selectable or verified generation and does not count toward
retention. Each root-lock owner opportunistically removes leftover garbage without
reading its markers; interrupted or partial deletion cannot poison later selection.

`prepare-dependencies.py SOURCE_DIR SNAPSHOT_DIR MANIFEST_PATH GENERATION KEY`
checks the source's combined public/private key and generation marker before linking. It copies
the private/vendor overlay, hardlinks installed files from the immutable generation,
and redirects workspace symlinks using their original clone-relative locations,
not the cache parent. An `EXDEV` hardlink failure emits an immediate warning before
copying; the final warning and `dependencyFiles.linked` / `dependencyFiles.copied`
manifest/receipt fields count actual hardlinks and fallback copies.
`dependencyKey` in the manifest and receipt identifies the selected generation.
After all dependency trees are copied, every dependency symlink must resolve beneath
the pinned snapshot or selected generation. A workspace ancestor that redirects a
link outside those roots fails the build, even when the stored link itself is relative.
Production state, manifests, snapshots, private overlay and credentials remain
deployment-owned. The helper stages `current`; it never prunes immutable builds
or publishes Devnet. The integrator alone prunes current plus one rollback build
after the published edge confirms activation.

The EXIT trap changes to the stable build root (or `/`) before removing any
snapshot. Deployment command wrappers can require a valid cwd even for absolute
arguments. Recoverable temporary-file, snapshot/lock removal and log-append
failures emit warnings without turning a successful build into a failed tick;
warnings available before the JSONL append appear in `cleanupWarnings`. A failed
log append is necessarily stderr-only. Failed-build rollback failure or inability
to enter any stable cwd remains an error because cleanup cannot establish safe
state. Artifact/current-pointer validation remains a hard build failure.
Failed-build cleanup deletes the new artifact only after verifying that neither
`current` nor `current-sha` selects it and rollback did not fail. Failed rollback,
an unresolved pointer or a still-selected revision preserves the artifact, emits
an unsafe-state warning, records `rollback-failed`/`rollback-inconsistent`, and
exits nonzero. Retaining a failed-but-selected directory is safer than creating a
dangling production pointer; operator recovery must reconcile that state.

The helper uses `gate-slot` admission by default; `FRACTAL_BUILD_ADMISSION=fallback`
explicitly reuses a caller's admission timeout and builds with `nice -n 19`.
The deployment supplies Git, GNU coreutils, `flock`, Python and the private
build entry point. Node comes from the candidate web shell inside the sandbox.
These scripts do not provision the private build code.

During an operator-approved cache rollout, any `snapshot-failed` or `build-failed`
tick, or a first built tick above 400 MB of process `write_bytes`, requires reverting
to the deployment's copy-mode helpers. This is an operator rollback criterion,
not an automatic fallback that hides a failed immutable-cache build.

## Tick observability

Every tick reads `servedSha` from `/build-identity.json` (`rev`) at the published `devnet url <purpose> --remote` edge.
`current` requires no `error`, a non-null `sha`, and equality with `servedSha`, not a disk build.
A failed lookup/fetch/identity decode sets `servedSha=null`, `current=false`, and a short `servedError`; lock-held ticks are false.
The `--local` probe records `localServedSha`/`localServedError` for diagnostics only; it never determines `current`.
`build` is `built`, `skipped`, `failed`, or `dry-run`; it is not a freshness signal.
Skipped `reason` is `unchanged`, `lock-held`, or `known-built-tree`/`known-failed-tree`.
Dry-run `reason` is `merged-and-regenerated-no-build-no-swap`; errors retain `build=failed`.
Observability alerts when the last `current=true` timestamp is over 24 hours old, or no tick arrives for 30 minutes.

## Follower inputs

Set `FRACTAL_FOLLOW_CLONE` to a dedicated main clone and `FRACTAL_FOLLOW_LOG` to
its JSONL log path, then run `bash apps/fractal-web/scripts/devnet/follow.sh`.
The follower checks unresolved member directories after every successful
fast-forward (including an unchanged HEAD). It installs after member application
or changes to `pnpm-lock.yaml`, `pnpm-workspace.yaml`, `megarepo.kdl`,
`megarepo.lock`, or `pnpm-source-inputs.json`, using:

```sh
nix develop .#web -c pnpm install --frozen-lockfile --prefer-offline
```

Successful member and install input hashes persist beside the log as
`<log>.members-key` and `<log>.install-key`. Member application also creates
`<log>.install-pending` until installation succeeds. A failed apply or install
therefore retries on later ticks even with unchanged HEAD or after restart;
JSONL records each failure's output. Keep these deployment-owned state files
alongside the log when restarting the follower.
The integrator's installed-input key covers the same five files plus the sandbox
policy and retains `<clone-name>-lockhash`; member-materialization keys include the
same policy. These source-install invalidation keys do not alter generation keys.

The deployment provides GitHub/Git access, Devnet and build credentials, and the
`git`, `gh`, `nix`, `pnpm`, `node`, `curl`, Python and shell tooling needed by the
integrator and its external build helper. The follower needs Git, Nix, Python
and GNU coreutils. Production state, credentials, private overlays and machine
configuration stay outside this repository. Deployment owners review and activate
new script revisions explicitly; committing these scripts does not restart a service.

## Candidate sandbox and residual risks

The trusted launcher realizes bubblewrap, Bash, coreutils, Git and Python from the
`nixpkgs` revision in its own `flake.lock`, not the candidate lock or host PATH.
Linux user namespaces and the pinned tools are required; a configuration or
sandbox failure stops the tick/build without an unsandboxed retry.

Candidate genie/regeneration, member application, pnpm install/lifecycle scripts,
license regeneration, Vite/SSR builds and precompression run in bubblewrap.
The initial environment is an explicit allowlist: PATH, scratch HOME/TMPDIR,
locale, CI, dedicated pnpm cache, candidate-local `MEGAREPO_STORE` and disabled
global/system Git configuration. Dev-shell definitions and hooks execute inside
the boundary; the final command receives the same runtime allowlist plus the
dev-shell PATH. Agent credentials and agent-policy variables are not inherited.

The filesystem exposes only the writable candidate clone/snapshot and dedicated
pnpm cache, read-only `/nix/store`, read-only Git metadata, resolver/CA material,
private `/proc` and `/dev`, and scratch `/tmp`/HOME. Host HOME, SSH agents,
`/run/user`, agent sockets and the Nix daemon socket are not mounted. Locked
members live beneath `<clone>/.devenv/candidate-members`; snapshots copy only selected
member source trees, excluding Git metadata and node_modules, and rewrite workspace
links locally. Every installed dependency tree comes from the verified immutable
generation, including node_modules beneath the copied member store.
Builds read-only bind every `node_modules` directory, the copied member store and
the pnpm cache, so dependency hardlinks cannot write through via a writable alias.

Build outputs are physically staged inside the snapshot and exposed at the
requested output path only inside bubblewrap. After success, trusted orchestration
rejects symlink artifacts and promotes the directory to the production build root.
Candidate commands cannot write production state or activate Devnet. Control-plane
Git/GitHub, admission, receipt checks, publication and retention remain outside.

Genie and builds have a private network namespace. Member fetching, installation
and license regeneration explicitly share the host network for dependency access.
These networked commands can contact public endpoints and host-reachable services,
including loopback: network egress filtering is not provided by this boundary.
The writable package cache is shared across candidates and remains an integrity
and availability surface; no agent credentials may be stored there.

### Known gaps

Candidate flake evaluation occurs outside bubblewrap with pure evaluation and a
clean bootstrap environment. Any build output defined by that flake is realized
by the trusted Nix daemon; those builds use the Nix build sandbox. The candidate
never receives a daemon socket, including when the deployment account is a
trusted Nix user. This is a residual Nix evaluation/build trust boundary, not a
claim that arbitrary candidate flake realization is performed by bubblewrap.

The production server runtime and follower are not changed by this build boundary.
The deployment still owns private overlay review, production credentials and
runtime service isolation. None of that state belongs in the public repository.
