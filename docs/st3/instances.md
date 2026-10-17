# Instances: a second st on one machine

`st --instance NAME` (or `ST_INSTANCE=NAME` in the environment) runs a second, fully separate
Fractal next to the default one. Use it to try a build without risking your working install, and
to see the first run again from a clean start.

```sh
st --instance trial setup
st --instance trial service install
st --instance trial agents ls
st --instance trial uninstall --yes --erase-local-graph
```

After instance startup, `agents ls` may report that its initial roster is still being
prepared. After changing an agent it can show the previous publication. A client that needs
its own write uses that instance daemon's receipt `store_index` as `min_cut` and checks the
served snapshot; a receipt from the default install or another host is not interchangeable.

`st --instance NAME setup` runs the first-run questions for the instance: it installs `st3`, `st` and
`pty` into the instance's own `bin/` (nothing goes on your login PATH or into `~/.local/bin`), and
the built-in Fractal Assistant works in the instance's work folder, `work/agents/`. Run the instance's commands from
there, or with `--instance`.

A name is 1 to 24 lowercase letters, digits and single hyphens. The flag works before or after the
subcommand and wins over the environment variable. Inside an instance, a different name is an
error, so a seat of one instance cannot start another by accident.

## Directory discovery

The daemon, CLI, standalone client and TUI share one resolver for product config
and state directories under `fractal`, falling back to `st3` under the same XDG base (`~/.config` and
`~/.local/state` by default, including on macOS). Instance roots resolve
`~/.fractal-instance/NAME`, falling back to `~/.st-instance/NAME`; selection is
per name, and `--instance` still wins over `ST_INSTANCE`.

The new directory is selected only if its
`.fractal-migration-completed` file contains exactly
`arn:lmig:smalltalk:2026-10-16-state-dirs-fractal` followed by a newline.
This receipt is required even when the old directory is absent: an unreceipted
destination may be a torn copy or foreign data. Otherwise the old path is used,
with one warning per process when an unreceipted destination exists. A receipt
is local to the directory it certifies, not its parent or another instance.
Only the migration's verified completion may write it.

Discovery does not move files or create directories. Creators use the same
selected root as readers, so a new config file is always read back. Without
a completed receipt that root remains the old path. Explicit config/state paths are unchanged.
Instance workspaces remain under `~/.st-instance/NAME/work`; workspace
relocation is separate. The external installers, hooks and Home Manager
declarations still use their existing paths; this reader expansion does not
authorize moving a live installation.

## What an instance owns

The legacy layout is below `~/.st-instance/NAME` (discovery above also accepts
an existing migrated root, except that `work/` stays at its legacy location):

| Directory | Holds | Set through |
| --- | --- | --- |
| `state/` | daemon state, graph, driver state | `XDG_STATE_HOME` |
| `config/` | `config.toml`, fleet file | `XDG_CONFIG_HOME` |
| `data/` | install manifest, Claude plugin marketplace | `XDG_DATA_HOME` |
| `cache/` | derived caches | `XDG_CACHE_HOME` |
| `run/` | daemon and client sockets | `XDG_RUNTIME_DIR` |
| `bin/` | the `st3`, `st` and `pty` that the installer puts there | |
| `claude/` | the instance's Claude account, plugins and `st` skill | `CLAUDE_CONFIG_DIR` |
| `codex/` | the instance's Codex account and `st` skill | `CODEX_HOME` |
| `home/` | the skill copy for pi, omp and opencode (`home/.agents/skills`) | |
| `work/` | the instance's work root: default workspaces for new agents in `work/agents/NAME`, including the Fractal Assistant | |
| `app/` | the macOS app bundle | |

The instance is applied to the environment once, at startup, before any thread exists. The daemon
and everything it starts (seats, harnesses, hooks) inherit it, so code that reads `XDG_*_HOME`
itself lands in the instance without knowing about instances. Seats of an instance sign in to
Claude and Codex separately from your own; that is what makes the first run clean.

Beyond directories, an instance has its own names:

- **Machine name:** the host's name plus the instance name (`laptop-trial`), so a fleet sees two
  machines.
- **systemd units:** `st3-instance-NAME.service` and `st3-instance-NAME-replication.service`. The
  prefix keeps an instance named `replication` from colliding with a default unit. The unit files
  sit in your real `~/.config/systemd/user`, because systemd reads nowhere else, and carry the
  instance's environment.
- **launchd labels:** `com.compoundingtech.st3.instance.NAME` and
  `com.compoundingtech.st3.instance.NAME.replication`, with plists in your real
  `~/Library/LaunchAgents`.
- **macOS bundle:** the installer puts `SmallTalk.app` at `~/.st-instance/NAME/app` with the bundle
  identifier `com.compoundingtech.smalltalk.instance.NAME`, and rewrites only the instance's launchd
  plists.

A bare `HOME` override is not a substitute. On macOS, launchd labels and the app bundle do not follow
`HOME`, so two installs would load and rewrite each other's services.

The service manager belongs to your real user session, so commands to `systemctl` and `launchctl`
get your own `XDG_*` values back. An instance never enables lingering, which is account-wide.

## Installing

```sh
scripts/install --instance trial            # from a source build
./install.sh --instance trial               # an extracted release
```

`--bin-dir` still wins over the instance's directory. After the install the instance runs through
`~/.st-instance/NAME/bin/st`, or any `st` with `--instance NAME`. Your login `PATH` is not changed.

## Uninstalling

`st --instance NAME uninstall` removes the instance's service definitions and the one directory, except
the work folder (`work/`) when anything is in it: that holds what your agents made, so uninstall
keeps it, says so, and removes everything else. An empty work folder goes with the
instance. Then the empty `~/.st-instance` goes when it was the last. It refuses to run if anything it would remove lies
outside that directory (for example a `state_dir` set elsewhere in the instance's `config.toml`).
`--dry-run` lists what it would remove.

`st uninstall` on the default install also removes: the copies of
the `st` skill that seats install into `~/.claude/skills` and `~/.agents/skills` (only when
unchanged), st's Claude plugin and marketplace registration, and `~/.local/share/st/plugins`. It
still leaves the work folder (`~/st`, see below) and what is in it, the account-wide lingering
setting, and the managed Claude Code policy, which needs root (`st3 claude-channel
uninstall-policy`).

## Proof

`scripts/st3-instance-isolation/run ST3 PTY [OUT]` starts the default st in a throwaway `HOME` with a
scrubbed environment and installs its skill, snapshots every path, mode, link target and file hash,
then starts an instance, installs its skill, uninstalls it and snapshots again. The snapshots must be
identical and `~/.st-instance` gone. It then removes the default install and requires no file to
remain. It uses no service manager and never reads or writes the machine's real install. Run it
outside a seat's process tree, because a daemon refuses a process that descends from another seat.

`scripts/install-macos-test` covers an instance install beside a default one whose plists,
app and commands stay byte-identical.

A harness started by hand inside an instance still reads its usual `HOME`-relative files
(for example `~/.gitconfig`). Only the directories above are redirected. Codex reads skills
from `CODEX_HOME/skills`; pi, omp and opencode read `home/.agents/skills` when launched with
that instance home.

## The work folder

Every install has one work root, the folder agents work in unless you point one at another folder.
It is `~/st` for the default install and `~/.st-instance/NAME/work` for an instance. New agents get
a folder inside it, `agents/NAME`, never the home folder, so an agent that lists its working
directory does not make macOS ask for access to Documents, Desktop or Downloads. `st setup` creates
the work root and prints it once. Change it with `work_root = "/absolute/path"` in `config.toml`; it
must be absolute and cannot be the home folder or inside Documents, Desktop or Downloads. Existing
agents keep their folders, and an agent or mission can still name any folder. `st uninstall` never
removes what is in the work root: it prints `kept` with the path (and `--dry-run` prints `keep`).
