# Model accounts

A person may own several accounts with one provider, because they use more than one account's worth
a month. On a shared node, each person's seats run on that person's own accounts. One design covers
both: an account has an owner, a seat binds an account or a pool of its owner's accounts, and a seat
at its limit moves to another account in its pool.

A fleet that declares no account is unchanged. A seat whose harness block binds nothing runs on the
harness's default login, exactly as before.

## Declare an account

An `account` is a root declaration, applied like an agent: `st agents apply accounts.kdl --as person/ada`.

```kdl
version 2

account "ada/claude-1" {
  provider "anthropic"
  owner "person/ada"
  plan "max"
  login "~/.claude-accounts/ada-1"
}

account "ada/claude-2" {
  provider "anthropic"
  owner "person/ada"
  plan "max"
  login "/srv/logins/ada-2" host="alder"
  login "~/.claude-accounts/ada-2"
}
```

- `provider` is `anthropic` (Claude Code) or `openai` (Codex). Other providers can be declared but no
  st harness uses them yet.
- `owner` is a person. A pool is the accounts one person owns.
- `plan` is a label for people and reports.
- `login` is the credential directory a harness is launched with: `CLAUDE_CONFIG_DIR` for Claude Code,
  `CODEX_HOME` for Codex. It is a path, never a credential; st never reads, prints or stores what is
  in it. A path is absolute or starts with `~/`, which is the home of the user the daemon runs as. A
  `host=` property gives a different directory per host; a login with no host serves every host. An
  account is only used on a host that has a login for it.

Sign an account in once, in its own directory, as the person who owns it:

```sh
CLAUDE_CONFIG_DIR=~/.claude-accounts/ada-1 claude    # then /login
CODEX_HOME=~/.codex-accounts/ada-1 codex login
```

A seat refuses to start on a login directory that does not exist, instead of falling back to the
default login. The directory is the harness's whole home: Claude keeps its login, `settings.json`,
plugins and `projects/` sessions there, Codex its `auth.json`, `config.toml` and `sessions/`. Copy
into it the settings a seat should have. Signing in with the variable set, as above, is what ties
the login to the directory on Linux and macOS alike; st only sets the variable.

## Bind a seat

```kdl
agent "ada/planner" {
  workspace "/work/app"
  harness "claude" {
    account "ada/claude-1"          // exactly this account
  }
}

agent "ada/builder" {
  workspace "/work/app"
  harness "claude" {
    account-pool "person/ada"       // any of person/ada's anthropic accounts
  }
}
```

Only `claude` and `codex` harness blocks take a binding. st sets the harness's login variable and
`ST3_ACCOUNT` on the seat, so an `env` entry for the same variable is refused. A restart on the same
account continues the native session the driver last bound. The session binding records the
account's name, so a switch to another account starts fresh and never copies the old account's
transcript into the new login directory.

A pooled seat starts on the account in its pool with the most usage left: the lowest weekly
percentage, then the lowest 5-hour percentage. An account no seat has reported yet counts as unused,
and so does one whose weekly window has reset. The seat keeps that account across restarts. The
choice is the node's own and lives in a local table: only the node that starts a seat has the
logins.

## Usage and limits per account

A bound seat's limits readings carry the declared account's name (`account_ref`) next to the
opaque account label, so `st usage` and stui name the account:

```
LIMITS  2 · the freshest reading of each account
WEEKLY  5-HOUR  WEEKLY RESET  MEASURED  account
97%  ?  2026-10-05 21:34 UTC  2026-10-02 21:34 UTC  ada/claude-1 (claude/aaaa000000000001)
```

Spend rows by account carry the same name. Seats that run on no declared account read as before.
An account keeps its last limits reading after its last seat switches away, until its window
resets. Declared accounts have separate readings even when the provider reports no account label.
Their spend and limits use a stable label derived from the declared name; unbound seats keep the
provider's label. Queued observations capture the account at their source, so replay after a
switch still charges the account that produced them.

## At the limit

`[limits]` in the node's config stops an account's seats at its weekly percentage (README). With
accounts:

- A seat bound to one account stops, as before.
- A seat bound to a pool restarts on another account in the pool whose weekly reading is under the
  percentage, the one with the most left, instead of stopping. It is switched once per weekly
  window. If every account in the pool is at its limit, it stops like any other seat.
- The operations notification names the seats affected by stops. Pool switches do not send stop alerts.

The selected account, fenced restart request and handled marker commit in one transaction. An
interrupted switch can retry without losing its restart request. Enforcement checks the seat's
current account binding and runtime incarnation before acting on a reading.

A switch is a fresh start: a harness cannot resume a native session from another account's
directory, and a seat's work lives in the graph. For that reason a pooled seat cannot declare
Claude session flags (`--continue`, `--resume`, `--session-id`) or Codex `resume`/`fork`
subcommands in its `args`. Codex configuration overrides (`-c`) are allowed.

## Not yet

Account declarations and seat bindings pass through smallclaims' existing rules for
`intent.desired`, including namespace rules in audit mode. Account-specific grants (who may start
seats on a particular account, under which namespace) still await smallclaims' content-aware
grants; its current rules match actor, kind and subject. Once those grants land, account use will
use them in audit mode. Once sekrets holds model logins, an account will name the sekrets profile
that holds its login instead of a directory.
