# Sekrets

Sekrets runs any command line tool with a credential the caller never reads. A seat asks for
`gh pr create`; a gateway owned by its own Unix user decides whether this caller may run that
command with that profile, runs it with the profile's home and environment, and hands back only
the command's output and exit status.

Sekrets is opt-in and Linux-first. A host without a gateway behaves exactly as before:
`sekrets` says sekrets is not set up, and nothing else changes. The design is section 10 of the
resource graph plan; this document describes what is built.

`sekrets` is its own binary, shipped beside `st` in every release: the gateway (`sekrets
serve`) and the command people and seats run. It is built from `crates/sekrets` and depends on
nothing of st. st's part is in the daemon: it vouches for its seats, records each gateway's log
as local observations, and makes GitHub API requests through the gateway
(`st3::sekrets::authorized`). `sekrets` asks the daemon to vouch for a seat by running `st`
(the seat's `ST3_BIN`), which knows its own daemon.

## Model

- **Gateway.** `sekrets serve`, run by systemd as the Unix user `sekrets`. It owns the store
  (`/var/lib/st-sekrets`, mode 0700) and listens on `/run/st-sekrets/gateway.sock`. Anyone on the
  host may connect; the kernel says who connected.
- **Profile.** One identity's credentials and a command policy, owned by a person:
  `OWNER/NAME`, such as `ada/gh` for Ada's own GitHub account and `ada/agent-gh` for the account
  her agents use. A profile has a home directory inside the store, where a tool's own login
  writes (`gh auth login` writes `~/.config/gh`), and values put into it that its commands get as
  environment variables (`GH_TOKEN`). A person may own any number of profiles.
- **Policy.** Which argument vectors a profile, or a grant of it, allows. Rules match whole
  arguments, never text inside them. An allow rule is a prefix (`gh pr create`); a deny rule is a
  prefix, optionally with options it refuses after the prefix (`gh pr create --body-file,-F`). A
  command is allowed when some allow rule matches and no deny rule does. `*` matches exactly one
  argument. Options before a subcommand (`gh -R other/repo pr create`) match no allow prefix
  unless one names them. Presets make a policy one word; `sekrets presets` lists them.
  `gh-agent` covers what agents use gh for (pull requests, issues, runs, workflows, releases,
  search, labels, variables and `gh api`) and never `gh auth token`, `auth status -t`, `secret`,
  `alias`, `extension` or `config`.
- **Passed files.** A deny rule can name file options, which read the file their value names
  (`--body-file`, `--input`), and field options, which read one after `@` (`-F key=@file`).
  Those are allowed only with the caller's standard input (`-`) or a file the caller passed. For
  gh, `sekrets` opens each such file itself, as the caller, passes it as a descriptor and
  rewrites the argument to `/dev/fd/N`, so `gh pr edit 7 --body-file /tmp/notes.md` works as
  written while no argument can make the command read a file of the sekrets user's, such as the
  profile's own login.
- **Grant.** The owner gives an agent, a pattern of agents (`agent/web/**`) or another
  person the use of a profile, with a policy and an optional expiry. A call through a grant must
  pass both the profile's policy and the grant's, so a grant can only narrow. A grant to an agent
  must list subcommands: a rule that allows a whole command (`gh`, `*`) is refused, because a
  command run through sekrets can read its profile's home, and no deny list anticipates every way
  a tool prints its own credential.
- **Log.** Every call, its exit, every refusal and every change to a profile, grant, lock or key
  is an entry in the gateway's log. Each person's daemon records the entries about its own seats
  and person as claims on `sekret/HOST/PROFILE` (`sekret.called`, `sekret.exited`,
  `sekret.refused`, `sekret.changed`). Entries and claims carry who called, which profile, the
  arguments and the exit status. They never carry a credential or a command's output. Arguments
  are recorded, so pass a secret with `sekrets put`, never as an argument.
- **Retention.** Nothing is kept forever. The claims are local observations of the gateway's
  host: they age out with the local observation log (`observations.retention`, seven days by
  default) and, when `[observations.otlp]` is set, go to OpenTelemetry for longer history. The
  gateway keeps its own log for seven days, long enough to bridge a daemon that was offline.

## Who is calling

The gateway checks the caller three ways, and the caller's own word counts for nothing:

1. **The kernel.** The socket's peer credentials give the caller's Unix user and process. Root's
   configuration maps each Unix user to a person. A process in that user's login session scope
   (`/user.slice/user-UID.slice/session-N.scope`, made by logind for a terminal or ssh login) is
   the person: an unprivileged process cannot move itself into a session scope.
2. **The daemon.** A process in the user's service manager (`user@UID.service`) may be a seat.
   `sekrets` asks the seat's daemon to vouch for it: the daemon reads the process's cgroup,
   finds the seat whose terminal it started in that scope (the terminal registry's
   `st3.scope-unit` tag beside its `st3.subject`), checks the process's ancestry names the same
   agent, and signs a statement with its node key. The statement names the agent, its person, its
   declaration revision, the cgroup, the process and its start time, and a nonce the gateway
   issued for this connection, and it counts for one minute.
3. **The chain.** The person registered that node key from a login session (`sekrets
   enable`), and the statement's person must be the person the Unix user maps to.

A process that is neither is refused as unidentified. A person's own terminal inside st (a seat's
terminal, or a terminal st started) is in the service manager, so the person calls from a login
session.

While seats run as their person's Unix user, the line between seats of the same person is only as
strong as that user's files: a seat that reads its daemon's node key, or moves itself into another
seat's cgroup, can pass for another seat of the same person. It can never pass for the person, for
another person, or for another person's seat, and it can never read the store. Seats under their
own Unix user (plan phase 8) close that. The same holds for a seat that logs in to its own host
over ssh with a key it added to its person's `authorized_keys`: it then runs in a login session.
Until seats run as another user, keep a person's own profile on a policy that leaves out the
commands that print credentials (`no-credential-printing`), and remove credentials from the
person's home (`sekrets adopt`, the next step).

## Authorized requests

The st daemon, and st commands such as gates, call GitHub's API themselves. With a sekrets
profile they do it without ever holding the token: each request goes to the gateway, which adds
the profile's token and returns the response. It is one request and one response over the
gateway socket, not a proxy.

- The caller is `host/NODE`: the process signs a statement with the node key its person
  registered (`sekrets enable`), bound to its pid, cgroup and the gateway's nonce, as a seat's
  daemon does for a seat. A person in a login session can also call with their own profiles.
- The profile and the grant must allow `http METHOD github`; the `github-api` preset allows any
  method. Grant it to the node: `sekrets grant ada/daemon-gh --to host/alder --preset github-api`.
- Only URLs under the gateway's `github_api` base (`https://api.github.com` unless root's
  configuration says otherwise) are reachable. A request that carries its own `Authorization` or
  `Cookie` header is refused; host and hop-by-hop headers are dropped; the gateway adds
  `Authorization: Bearer TOKEN`.
- Redirects are never followed: a 3xx comes back as it is. Every response header (ETag, Link,
  rate limits) and the raw body come back; a 4xx or 5xx is a response, not an error. Nothing is
  retried. Request bodies are limited to 16 MiB and response bodies to 64 MiB; each request has 60
  seconds.
- The token is a value put into the profile (`GH_TOKEN` or `GITHUB_TOKEN`), else the profile's gh
  login, read once and kept until a 401 says it changed.
- Each request is logged by method, path and status, never with the token or the body.

In Rust: `st3::sekrets::authorized::authorized_request(&config, "ada/daemon-gh", &request)`
returns the response or `Unavailable` (no gateway or no node key), `Refused` or `Transport`.

## The sandbox

The gateway runs each command as the sekrets user inside bubblewrap:

- The system is read-only. The store, the gateway socket's directory, `/home`, `/root`, `/tmp`
  and each configured checkout root are hidden.
- The profile's own home is bound read-write; no other profile's is visible.
- The caller's working directory, and in a git checkout its git directories, arrive as directory
  descriptors the caller opened and passed over the socket, and are bound read-only at the paths
  the kernel gives them, which must lie under a checkout root. The gateway never opens a path the
  caller names. bubblewrap binds a descriptor by the path it resolves to, so the sekrets user must
  be able to pass through the directories above the checkout: setup grants it search, never read
  or list, on each person's home (`setfacl -m u:sekrets:x HOME`). A seat can read nothing more,
  and the command sees no home but the checkout. From a directory the gateway cannot reach or does
  not serve, the command runs in the profile's home, and the caller is told so.
- A repository's own configuration can run programs (`core.fsmonitor`, `filter.*.clean`,
  `pager.*`, hooks, `credential.helper`, `url.*.insteadOf`, includes). Inside the sandbox each git
  directory's `config` and `config.worktree` are replaced by a copy that keeps only data
  (remotes, branches, identity, format extensions), and its `hooks` and `modules` directories by
  empty ones. Command-line configuration then turns off hooks, the file system monitor, local and
  external transports, submodule recursion and signing, and git looks for a repository no higher
  than the checkout. A `.git` that is a symbolic link, or a gitdir file or `commondir` naming a
  directory outside the passed checkout, is refused: git would follow it to a configuration the
  gateway never sanitized.
- The checkout is read-only to the command: it runs as the sekrets user, which owns none of the
  caller's files. `git push` through sekrets pushes the branch and exits 0 but cannot record the
  remote-tracking ref or `-u` upstream in the checkout; run `git fetch` and
  `git branch --set-upstream-to` as the caller afterwards. Commands that write files into the
  checkout (`gh pr checkout`, `gh repo clone`, `gh run download`) do not work through sekrets.
- The command and every tool it runs come from the gateway's configured path. Each directory and
  file on the way must belong to root (or the sekrets user) and be writable by no one else, so no
  person or seat can change what runs as sekrets. A command is a name, never a path.
- The environment is built from nothing: the profile's values, `HOME`, `PATH`, `TERM` on a
  terminal, and the git settings above.
- Without a terminal the command gets the caller's own standard streams, passed as descriptors,
  so its output never passes through the gateway. With one, the gateway makes a terminal and
  passes its controlling side to the caller, which relays it.

A command that runs code from the checkout (a build script, a package manager's hooks) can at
worst use the one profile it runs as. The presets allow only `gh` and `git` subcommands that do
not; a policy that allows more takes that risk on purpose.

## Commands

```sh
sekrets setup --person person/ada > sekrets-setup.sh   # review, then: sudo sh sekrets-setup.sh
sekrets enable                                          # from a login session
sekrets profile create ada/gh --preset everything --preset no-credential-printing --default
sekrets login gh --profile ada/gh                       # gh's own device-code login, as sekrets
sekrets profile create ada/agent-gh --preset gh-pr --preset git-push
printf %s "$TOKEN" | sekrets put GH_TOKEN --profile ada/agent-gh
sekrets grant ada/agent-gh --to 'agent/web/**' --preset gh-pr --preset git-push --until 2026-11-01
sekrets -- gh pr list                                   # a person: their default profile
sekrets -- gh pr create --draft --title "..."           # a seat: the one granted profile that allows it
sekrets lock --reason "token leaked"                    # stop all use; sekrets unlock lifts it
sekrets log --follow
```

`sekrets setup` prints the root script: it creates the `sekrets` user and its store, copies
this `sekrets` binary to `/usr/local/libexec/sekrets` owned by root, writes
`/etc/st-sekrets/gateway.toml` (Unix user to person, the tool path, the checkout roots) and
installs and starts `st-sekrets.service`. Run it again after updating st to give the gateway the
new binary. A seat's `gh` can be a one-line shim, `exec sekrets -- gh "$@"`.

Changes to profiles, grants and keys come only from the owning person in a login session. A seat
can run, list what it may use, read its person's log and lock; it changes nothing else.

## What is not built yet

- Profiles, policies and grants live in the gateway's store and are mirrored to the graph as
  `sekret.changed` claims; the gateway does not yet read grants from the graph. The smallclaims
  rules (`st rules`) restrict writes and have no grants to read from; when signed grants land,
  the gateway will take its grants from them.
- `sekrets adopt`, which moves existing logins out of the person's home, is the next step.
- A model API proxy, macOS, and seats under their own Unix user are later phases.
