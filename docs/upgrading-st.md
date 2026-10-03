# Upgrade Small Talk safely

Upgrade each machine in your own fleet to the **same release**. Keep the old archive and read the new release's compatibility notes before starting it against your state. For an initial installation, use [getting started](getting-started.md).

## Install a pinned build on each machine

Check the current services and graph first:

```sh
st service status
st doctor
st replication status
st agents ls
```

Before swapping binaries, copy the database aside. This example uses the default state directory (adjust `st_state_dir` if you configured another) and Python 3's SQLite backup API, which includes committed writes still in the WAL. A plain copy of a live `.sqlite3` file is not enough:

```sh
st_state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/st3"
st_backup_dir="$HOME/smalltalk-backups/$(date -u +%Y%m%dT%H%M%SZ)"
umask 077
mkdir -p "$st_backup_dir"
python3 - "$st_state_dir/claims.sqlite3" "$st_backup_dir/claims.sqlite3" <<'PY'
import pathlib, sqlite3, sys
source_path = pathlib.Path(sys.argv[1]).resolve()
with sqlite3.connect(source_path.as_uri() + "?mode=ro", uri=True) as source:
    with sqlite3.connect(sys.argv[2]) as backup:
        source.backup(backup)
        assert backup.execute("PRAGMA quick_check").fetchone()[0] == "ok"
print("Database copied to", sys.argv[2])
PY
for directory in keys fleet; do
  if [ -d "$st_state_dir/$directory" ]; then
    cp -R "$st_state_dir/$directory" "$st_backup_dir/"
  fi
done
```

Keep this private copy and the machine's configuration safe. It is recovery material, not a command to overwrite a running member's state.

Choose a tag from [releases](https://github.com/compoundingtech/smalltalk/releases). Run this on **each machine**, entering the same tag; it downloads without touching running services:

```sh
printf 'Release tag to install (for example v0.3.4): '
read -r st_tag
mkdir -p "$HOME/smalltalk-install/$st_tag"
cd "$HOME/smalltalk-install/$st_tag"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) archive=smalltalk-x86_64-unknown-linux-gnu.tar.gz ;;
  Darwin-arm64) archive=smalltalk-aarch64-apple-darwin.tar.gz ;;
  *) echo 'Use Nix on this platform'; exit 1 ;;
esac
release="https://github.com/compoundingtech/smalltalk/releases/download/$st_tag"
curl -fLO "$release/$archive"
curl -fLO "$release/$archive.sha256"
if command -v sha256sum >/dev/null; then
  sha256sum -c "$archive.sha256"
else
  shasum -a 256 -c "$archive.sha256"
fi
tar -xzf "$archive"
"./${archive%.tar.gz}/install.sh" --bin-dir "$HOME/.local/bin"
cat "${archive%.tar.gz}/BUILD.json"
```

`BUILD.json` records the exact source commit. Release tags are labels; `st --version` alone does not identify the source build. For Nix installations, use the same pinned source on all machines and your normal profile or Home Manager update procedure; see [installation](../README.md#install).

## Restart the services and verify

Finish the install on one machine before moving to the next:

```sh
st service install
st service status
st doctor
st agents ls
st replication status
```

`service install` refreshes executable paths and restarts the installed daemon and replication services. The daemon adopts live seats; supported drivers/channels follow its new binary. A provider's conversation can keep running through the deploy. Check that local delivery becomes `current`, and read an `outdated` or `stale` reason before deciding a seat needs a restart.

After all members are upgraded and caught up, compare replication digests as [two machines](two-machines.md#check-what-arrived) describes. Mixed builds can exchange signed history while an older member has claims waiting for an upgrade; that does not prove equal projections. Check a real conversation and its message receipt too:

```sh
probe_message=$(st conversations send agent/garden/worker --from person/ada \
  --subject 'Upgrade check' --body 'Confirm you can read this after the upgrade.')
st conversations status "$probe_message"
st agents show agent/garden/worker
```

## Swap back or roll forward?

Swap back only when the older build is explicitly compatible with the **current database schema, claim vocabulary, and driver resume format**. Reinstall the saved archive, refresh the services, and repeat the same health checks. Do not assume a smaller version number can read state the newer build has already migrated.

If the release changes the schema without a supported downgrade, **roll forward** to a fixed build. Replacing executables does not reverse a database migration. Do not use `st service reset` as a rollback: it erases local state. A database restore is a separate recovery procedure, with fleet history and writes since the backup to account for.

Claim backups are coming; use their published create/restore procedure once released. Replication provides another live copy, but it also shares new writes and is not a historical backup. Preserve workspace repositories and private host configuration separately from graph recovery.

See [binary releases](st3/binary-releases.md), [seats across deploys](st3/seat-deploys.md), and [replication recovery](st3/replication.md#recovery).
