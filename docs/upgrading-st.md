# Upgrade Smalltalk safely

Upgrade each machine in your own fleet to the **same release**. Keep the old archive and read the new release's **Upgrade impact** section before starting it against your state. Patch tags do not promise rollback or mixed-build compatibility; see the [0.x compatibility policy](st3/compatibility.md). For an initial installation, use [getting started](getting-started.md).

## Install a pinned build on each machine

If a populated v0.3.4 store may contain unsigned delegation grants, read the
[founder signing audit](st3/founder-signing-audit.md) **before** running doctor or restarting.
Preserve raw state and keys first; the normal diagnostics below are for stores without that risk.

Check the current services and graph first:

```sh
st service status
st doctor
st replication status
st agents ls
```

For a healthy current daemon, preserve a consistent raw database, private keys, fleet files
and configuration **and** make a [claim backup](st3/backups.md). Keep the previous installed
archive and its `BUILD.json` separately before changing any install directory; retain its
checksum and installed-version record too. A version record cannot replace the old executables.

Adjust `st_state_dir` if your configured state directory differs from this default. The SQLite
backup API includes committed WAL writes; copying only a live `.sqlite3` file does not. If keys
or configuration can change during capture, coordinate a consistent filesystem snapshot as
[the founder audit](st3/founder-signing-audit.md#preserve-evidence-before-running-diagnostics)
describes. The subshell keeps the restrictive umask out of your interactive shell:

```sh
st_state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/st3"
st_backup_dir="$HOME/smalltalk-backups/$(date -u +%Y%m%dT%H%M%SZ)"
(
  set -eu
  umask 077
  mkdir -p "$HOME/smalltalk-backups"
  mkdir "$st_backup_dir"
  python3 - "$st_state_dir/claims.sqlite3" "$st_backup_dir/claims.sqlite3" <<'PYTHON'
import pathlib, sqlite3, sys
source_path = pathlib.Path(sys.argv[1]).resolve()
with sqlite3.connect(source_path.as_uri() + "?mode=ro", uri=True) as source:
    with sqlite3.connect(sys.argv[2]) as backup:
        source.backup(backup)
        assert backup.execute("PRAGMA quick_check").fetchone()[0] == "ok"
print("Consistent database copy saved")
PYTHON
  for directory in keys fleet; do
    if [ -d "$st_state_dir/$directory" ]; then
      cp -R "$st_state_dir/$directory" "$st_backup_dir/"
    fi
  done
  st_config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/st3"
  if [ -d "$st_config_dir" ]; then
    cp -R "$st_config_dir" "$st_backup_dir/config"
  fi
  st backup create "$st_backup_dir/before-upgrade.jsonl"
  st --version --json > "$st_backup_dir/installed-version.json"
  st doctor --json > "$st_backup_dir/doctor.json"
)
```

**Stop the upgrade if this block fails.** If claim export refuses because the graph is stale,
projections are pending or history is incomplete, recover that condition and retry with a new
backup directory before installing or restarting. Do not proceed with a partial backup.

Keep both forms private: the raw capture preserves the original database and keys, while the
claim archive supports verified offline reconstruction under a fresh writer. The archive
excludes private keys, unsealed pending work and local runtime state; it does not replace the
raw capture. Preserve workspace repositories and native harness sessions separately. Neither
backup is a command to overwrite a running member's database. An offline claim export opens
and migrates its input: use an additional consistent SQLite copy, never the untouched original.

Choose a tag from [releases](https://github.com/compoundingtech/smalltalk/releases). Run this on **each machine**, entering the same tag; it downloads without touching running services:

```sh
printf 'Release tag to install (for example v0.3.16): '
read -r st_tag
mkdir -p "$HOME/smalltalk-install/$st_tag"
cd "$HOME/smalltalk-install/$st_tag"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) archive=smalltalk-x86_64-unknown-linux-gnu.tar.gz ;;
  Darwin-arm64) archive=smalltalk-aarch64-apple-darwin.tar.gz ;;
  *) echo 'This guide covers Linux x86_64 and Apple Silicon only'; exit 1 ;;
esac
release="https://github.com/compoundingtech/smalltalk/releases/download/$st_tag"
curl -fLO "$release/$archive"
curl -fLO "$release/$archive.sha256"
curl -fLO "$release/RELEASE.json"
curl -fLO "$release/UPGRADE-IMPACT.json"
if command -v sha256sum >/dev/null; then
  sha256sum -c "$archive.sha256"
else
  shasum -a 256 -c "$archive.sha256"
fi
tar -xzf "$archive"
"./${archive%.tar.gz}/install.sh" --bin-dir "$HOME/.local/bin"
cat "${archive%.tar.gz}/BUILD.json"
```

`BUILD.json` records the exact source commit. Release tags are labels; compare source revisions and build metadata when checking an upgrade. For Nix installations, use the same pinned source on all machines and your normal profile or Home Manager update procedure; see [installation](getting-started.md#1-download-and-open-st).

`st --version` names the source revision and whether the build came from Nix or local source.
`st --version --json` returns a stable `machine_version` without needing a daemon.
`st doctor --json` and client capabilities expose the responding daemon's `machine_version`,
so an installed CLI and a running daemon can be compared. All versions use metadata baked at
compile time, independent of the caller's directory or environment.

## Config keys and clients on an old PATH

After an install adds configuration keys, replace stale `st` clients on session PATHs and
restart the affected sessions through the normal coordinated lifecycle. Compare `st --version --json` in the session with the installed build and the daemon version from `st doctor --json`.
Replacing installed files does not update a session pinned to an older executable or Nix path.

New clients ignore unknown top-level and nested config keys with one stderr warning that lists
escaped key paths, never their values. This warning is once per process, including shell
completion: at most 16 paths, each capped at 160 characters, with explicit truncation notices.
`st doctor` lists the same bounded names and `--strict` fails on the warning. Known values
still use the existing typed parser and validation. Daemon startup, replication, policy reloads
and local configuration/state management (including fleet, uninstall, admission, service
status and Fabric discovery) remain strict about unknown settings. Setup rejects unknown keys before rewriting; it keeps
its existing raw-TOML merge, which can reformat comments/order rather than preserving the
original text. New restrictive client-visible settings must not rely on older clients honoring
them: those clients can ignore an unknown restriction with only this warning.

This change cannot repair clients already deployed with the older strict parser: the next
key-adding upgrade still breaks those old clients once. Any install that adds a key must call
out that limit and include the session/PATH upgrade instruction. This is not a general promise
that different daemon releases can run together, and no config-version field is introduced.

## Restart the services and verify

Schedule one coordinated upgrade window for the whole fleet, with the target files ready on
each machine. Restart and verify each member within that window; do not leave a mixed-rules
fleet as the final state. Mixed checkpoint rules prevent new sealing even when replication
still exchanges history. Follow any stricter order or manual steps in the release notes.

Installing files does not restart running processes. If [Home Manager](home-manager.md) owns
the daemon, update its pinned input and activate the configuration during this window; activation
refreshes paths and restarts the daemon. Skip the manual `st service install` below for that route.
For archive and manual Nix-profile services, run all the commands below after installation;
for Home Manager, run the verification commands after activation:

```sh
st service install
st service status
st --version --json
st doctor --json
st agents ls
st replication status
```

Ordinary `st agents ls` and `st agents tree` read the latest published roster without
requesting a refresh. Immediately after starting or changing an agent they can show the
earlier roster; after daemon startup the roster may still be being prepared. When you need
to see your own write, pass its same-host receipt `store_index` as `min_cut` to the agents
client read and check the served snapshot index. This is not a replicated or cross-host cut.

`service install` refreshes executable paths and restarts the installed daemon and replication services. The daemon adopts live seats; supported drivers/channels follow its new binary. A provider's conversation can keep running through the deploy. Compare the installed CLI's
`machine_version` with the responding daemon's `machine_version` in doctor; also compare both
with the archive's source metadata. An active service alone does not prove that the API is ready.
Recovery may rebuild projections or replay the whole graph before serving requests. Keep
`st service status` and `st doctor` as your startup checks; see [startup diagnosis](when-something-is-wrong.md#startup-replay-and-an-unavailable-api).

Replay duration depends on state and host load. The v0.3.15 notes record roughly eight minutes
without an API on one populated member and about eighteen minutes to health on two others;
these are observations, not an outage bound. See the [v0.3.15 release notes](https://github.com/compoundingtech/smalltalk/releases/tag/v0.3.15). A checkpoint rules bump alone does not prove a
full replay trigger. Backups and restart windows must accommodate the release's stated impact.

Check that local delivery becomes `current`, and read an `outdated` or `stale` reason before deciding a seat needs a restart.

After all members are upgraded and caught up, compare replication digests as [two machines](two-machines.md#check-what-arrived) describes. Mixed builds can exchange signed history while an older member has claims waiting for an upgrade; that does not prove equal projections. Check a real conversation and its message receipt too. The example uses the worker declared in [missions in practice](missions-in-practice.md#declare-a-worker-seat); substitute your own existing seat when upgrading:

```sh
probe_message=$(st conversations send agent/garden/worker --from person/ada \
  --subject 'Upgrade check' --body 'Confirm you can read this after the upgrade.')
st conversations status "$probe_message"
st agents show agent/garden/worker
```

## Swap back or roll forward?

Swap back only when the older build is explicitly compatible with the **current database schema, claim vocabulary, and driver resume format**. Reinstall the saved archive, refresh the services, and repeat the same health checks. Do not assume a smaller version number can read state the newer build has already migrated.

If the release changes the schema without a supported downgrade, **roll forward** to a fixed build. Replacing executables does not reverse a database migration. Do not use `st service reset` as a rollback: it erases local state. A database restore is a separate recovery procedure, with fleet history and writes since the backup to account for.

Claim backups are available. Rehearse [offline restore into a separate empty state directory](st3/backups.md#rehearse-a-restore)
before relying on one. Restore returns a fresh writer identity; it does not restore private
keys, start services or resume native sessions. Account for writes since the backup and join a
recovered node as a new member rather than reusing the original writer. Replication provides another live copy, but it also shares new writes and is not a historical backup. Preserve workspace repositories and private host configuration separately from graph recovery.

See [binary releases](st3/binary-releases.md), [seats across deploys](st3/seat-deploys.md), and [replication recovery](st3/replication.md#recovery).
