# Instances: a second st on one machine

`st --instance NAME` (or `ST_INSTANCE=NAME` in the environment) runs a second, fully separate
Smalltalk next to the default one. Use it to try a build without risking your working install, and
to see the first run again from a clean start.

```sh
st --instance trial setup --yes --person ada
st --instance trial agents ls
st --instance trial uninstall --yes --erase-local-graph
```

A name is 1 to 24 lowercase letters, digits and single hyphens. The flag works before or after the
subcommand and wins over the environment variable. Inside an instance, a different name is an
error, so a seat of one instance cannot start another by accident.

## What an instance owns

Everything below one directory, `~/.st-instance/NAME`:

| Directory | Holds | Set through |
| --- | --- | --- |
| `state/` | daemon state, graph, driver state | `XDG_STATE_HOME` |
| `config/` | `config.toml`, fleet file | `XDG_CONFIG_HOME` |
| `data/` | install manifest, Claude plugin marketplace | `XDG_DATA_HOME` |
| `cache/` | derived caches | `XDG_CACHE_HOME` |
| `run/` | daemon and client sockets | `XDG_RUNTIME_DIR` |
| `bin/` | the `st3`, `st` and `pty` that setup installs | |
| `claude/` | the instance's Claude account, plugins and `st` skill | `CLAUDE_CONFIG_DIR` |
| `codex/` | the instance's Codex account and `st` skill | `CODEX_HOME` |
| `agents/` | default workspaces for new agents, including the onboarding expert | |
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
curl -fsSL .../install.sh | sh -s -- --instance trial
```

`--bin-dir` still wins over the instance's directory. After setup the instance runs through
`~/.st-instance/NAME/bin/st`, or any `st` with `--instance NAME`. Your login `PATH` is not changed.

## Uninstalling

`st --instance NAME uninstall` removes the instance's service definitions and the one directory, then
the empty `~/.st-instance` when it was the last. It refuses to run if anything it would remove lies
outside that directory (for example a `state_dir` set elsewhere in the instance's `config.toml`).
`--dry-run` lists what it would remove.

`st uninstall` on the default install now also removes what it used to leave behind: the copies of
the `st` skill that seats install into `~/.claude/skills` and `~/.agents/skills` (only when
unchanged), st's Claude plugin and marketplace registration, and `~/.local/share/st/plugins`. It
still leaves workspaces you or your agents created (`~/st/agents`), the account-wide lingering
setting, and the managed Claude Code policy, which needs root (`st3 claude-channel
uninstall-policy`).

## Proof

`scripts/st3-instance-isolation/run ST3 PTY [OUT]` installs the default st in a throwaway `HOME`
with a scrubbed environment, snapshots every path, mode, link target and file hash, then sets up an
instance, starts its daemon, installs its skill, uninstalls it and snapshots again. The snapshots
must be identical and `~/.st-instance` gone. It then removes the default install and requires no file
to remain. It uses no service manager and never reads or writes the machine's real install.

Result on Linux for this change: both checks pass. The installers'
mocked macOS transactions also pass (`scripts/install-macos-test`), including an instance install
beside a default one whose plists, app and commands stay byte-identical.

## Not covered

- **macOS on a real Mac.** The installer and launchd changes are exercised by the mocked
  transaction tests and by unit tests of the names and plist rendering, not by a signed install on
  macOS or a real `launchctl` session.
- **systemd units for an instance.** Rendering and naming are tested; installing one into the real
  user manager was not done, because that touches the real session.
- **`st skill` for Codex, pi, omp and opencode** in an instance goes to `CODEX_HOME/skills` or a
  directory inside the instance. Whether those harnesses load a skill from there, rather than from
  `~/.agents/skills`, was not checked.
- A harness started by hand inside an instance still reads whatever `HOME`-relative files it always
  read (for example `~/.gitconfig`). Only the directories above are redirected.
