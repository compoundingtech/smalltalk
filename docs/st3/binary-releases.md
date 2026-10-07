# Smalltalk binary releases

Every pushed Git tag runs **Smalltalk tag release**. Both native builds must pass before a
GitHub release is published. A release is also published once a day when `main` changed since the
last one (see [Daily releases](#daily-releases)). Each release contains:

- `smalltalk-x86_64-unknown-linux-gnu.tar.gz`: Linux x86_64, built on Ubuntu 22.04 (glibc 2.35 or newer).
- `smalltalk-aarch64-apple-darwin.tar.gz`: Apple Silicon, macOS 15 or newer. The executables are
  ad-hoc signed, not Developer ID signed or notarized; macOS may require approval to open them.
- One `.sha256` checksum per archive, combined `SHA256SUMS`, and `RELEASE.json` with the exact
  source commit, target, Rust compiler version, and PTY runtime revision.

Each archive has `bin/st3`, `bin/st` (a relative symlink to `st3`), `bin/stui`, `bin/st3-migrate`,
and `bin/pty`, plus `install.sh`, `install-macos.py`, `BUILD.json`, and this guide. PTY is built from the exact
`flake.lock` runtime revision; this is distinct from the `pty-core` library dependency. These
archives need neither Nix nor Rust installed. Harness CLIs and their logins remain separate.

## Install or update

If a populated v0.3.4 store may have unsigned delegation grants, follow the
[founder signing audit](founder-signing-audit.md) before doctor or restart. Preserve raw state and
keys first. A build containing prevention #1269 can sign preserved unsealed work; it retains
signature warnings from already-sealed affected payloads.

Choose a tag from the repository's Releases page, then download the archive for your machine
and its `.sha256` file. For example, with GitHub CLI:

```sh
tag=v0.3.16 # example: choose a release and read its Upgrade impact first
archive=smalltalk-x86_64-unknown-linux-gnu.tar.gz
# On Apple Silicon: archive=smalltalk-aarch64-apple-darwin.tar.gz
gh release download "$tag" --repo compoundingtech/smalltalk \
  --pattern "$archive" --pattern "$archive.sha256"
shasum -a 256 -c "$archive.sha256"
tar -xzf "$archive"
"./${archive%.tar.gz}/install.sh" --bin-dir "$HOME/.local/bin"
```

Put the chosen bin directory on `PATH`. The installer stages all four tools and the `st` link,
then replaces each by rename; existing processes retain their old executable. It does not restart
anything or erase state. On macOS it installs `st3` and `stui` in the fixed `~/Applications/SmallTalk.app` bundle, registers it with Launch Services, and updates existing daemon/replication LaunchAgent paths. Configure `ST_MACOS_SIGNING_IDENTITY` and optionally `ST_MACOS_SIGNING_TEAM` for persistent signing; no identity uses ad-hoc signing. A configured missing identity fails rather than falling back. See [macOS installation and signing](macos-installation.md). Keep the previous archive as recovery material. Installing it again is a supported rollback
only when its database, claim and driver contracts can read the current state; see
[upgrade and recovery](../upgrading-st.md#swap-back-or-roll-forward) and the
[0.x compatibility policy](compatibility.md). Forward-only migrations require roll-forward
unless the release explicitly documents a downgrade. Before upgrading a healthy populated
store, make a [claim backup](backups.md) and preserve excluded local data separately.

Run `st service install` after the first install or a changed executable path; it refreshes
service definitions and restarts daemon and replication services. `st service restart` suffices
when those definitions already point at the intended binaries. Schedule that restart, compare
`st --version --json` with the daemon's `machine_version` in `st doctor --json`, and verify peer
health. A reachable doctor response and matching version identify the running daemon; they do
not certify store invariants. Until maintained invariant evidence is integrated, live doctor
lists unchecked invariants by name as `unknown`, reports `warn`, and `st doctor --strict`
always exits 2. Do not use strict success as an upgrade-readiness gate during this stage, or
interpret the warning as evidence that the API is unavailable. Strict still requires complete
evidence and fails on computed warnings and errors. An explicit offline audit preserves unknown
results for unsupported checks and cannot substitute for complete certification.

`st replication checkpoint status` also exits 2 while checkpoint comparison evidence is
uncomputed: its HTTP 503 `diagnostic-evidence-incomplete` response describes an uncertified
current set. It does not indicate a daemon outage or require a restart. Restarting
invalidates an ongoing continuous soak window, so coordinate that separately from downloading or
installing files.

## Build and publication proof

Every commit on `main`, PRs changing release files and manual **Run workflow** invocations run both native builds,
archive extraction, a temporary-directory installation, CLI help checks, the TUI PTY smoke suite,
and combined checksum/source validation. They upload `smalltalk-release` as an Actions artifact
(kept 7 days) and never publish a GitHub release, so release breakage fails on `main` and not at tag
time. Each native job also uploads its own archive and checksum as `release-<target>`, for example
`release-x86_64-unknown-linux-gnu`; once a main commit's run has succeeded,
`gh run download RUN_ID --repo compoundingtech/smalltalk --name release-x86_64-unknown-linux-gnu`
fetches that commit's Linux archive without building it (find RUN_ID with
`gh run list --workflow release-smalltalk.yml --branch main --commit SHA`). Tag pushes use the same jobs, then verify that the tag still
points at the built commit, upload a draft, and publish it only after every asset is present.
Existing releases are not overwritten. If publication fails leaving a draft, inspect and remove
that draft before rerunning the publish job; never move a published tag.

Native builds restore Cargo dependencies and Zig's cache on both platforms; the pinned PTY
runtime uses the same cached Cargo target directory. Generated `target`, `.zig-cache`, and `dist`
files and Python bytecode are ignored by Git, so restored caches and previous archives do not mark
the baked source version dirty. The extracted, installed binary must still report the exact clean
checkout version.

The separate **Nix** workflow builds the default Linux package and every native flake check on
tags, relevant trusted PRs, manual dispatch, and daily at 01:17 UTC. It uses ci1's persistent Nix
store, retains the latest outputs per runner as GC roots, and repeats the build with downloads
and both local and remote builders disabled to prove the results are cached. It needs neither
GitHub's Actions cache nor FlakeHub. The `nix-release-proof` artifact records the exact source,
targets, and output paths. The scheduled check starts four hours before the daily release schedule
to catch package breakage early.

The st3 Nix check uses nextest to give each test its own process, including CPU-budget and
delivery-presence fixtures. Python, Node, and the Linux process utilities are declared test inputs;
the historical messaging binary is pinned in the lockfile and supplied before the sandbox starts.
The native runtime suite and documentation tests remain covered.

## Daily releases

**Smalltalk daily release** (`release-daily.yml`) runs at 05:17 UTC and on manual dispatch. It takes the
newest `main` commit whose release run succeeded; if that commit is not already in the latest
release, it re-checks the checksums and sources of that run's `smalltalk-release` artifact, publishes
those same bytes under the next patch tag (the highest `vX.Y.Z` tag plus one; dispatch with `tag` to
choose another), and lists the merged pull request titles since the previous release in the notes.
Nothing is rebuilt, and the tag is created at that exact commit with the workflow's own token, so it
does not start a second tag build. There are no version-bump commits. Archives from these releases
carry no tag in `BUILD.json`; `source` identifies the commit. `scripts/release-smalltalk-daily
--dry-run` prints what would be published without publishing.

Daily, manual dispatch, and tag publication use the same source-pinned **Upgrade impact**
section before the change list, and attach `UPGRADE-IMPACT.json`. It describes replay,
database changes, checkpoint rules/fleet coordination, client and harness compatibility,
service interruption, manual steps, and recovery. Missing classifications stop publication;
main builds continue. PRs above the documented [adoption boundary](release-impact.md)
also need a fresh valid fragment before merging. See [release impact authoring](release-impact.md) for
committed fragments, measurements, historical backfills, and a notes-only preview.

Tag names are labels, not embedded package versions: use `BUILD.json`/`RELEASE.json` for exact
source identity. Tag the tested commit (with this workflow in its tree); pushing a tag does not
wait for unrelated CI runs. Tags made with another workflow's default `GITHUB_TOKEN` do not
trigger another push workflow, and GitHub does not emit tag push events for a push of more than
three tags: push release tags individually from a person or app credential.

The runner and trigger choices follow GitHub's [runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
and [push event rules](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#push).
The prior `release-portable.yml` remains the separate manual st2 release path.
