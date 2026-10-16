# Model accounts

A person may own several accounts with one provider, because they use more than one account's worth
a month. On a shared node, each person's seats run on that person's own accounts. One design covers
both: an account has an owner, a seat binds an account or a pool of its owner's accounts, and a seat
at its limit moves to another account in its pool.

A fleet that declares no account is unchanged. A seat whose harness block binds nothing runs on the
harness's default login.

## Declare an account

An `account` is a root declaration, applied like an agent: `st apply accounts.kdl --as person/ada`.

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

An agent declared inside a mission or step binds an account the same way, so a mission can run on
an allowance that is about to reset. Only the `account` declaration stays at the root: an `account`
that declares one inside a mission or step is refused with `account-inside-mission`.

A binding is also an authority fact, not only a choice of allowance. A bound account's owner (or a
pool's person) decides whose person the agent is: asks the agent raises and its own-ask authority
are attributed to that person ahead of the declaration author or the mission requester. A mission
agent that binds another person's account is therefore treated as acting for that person, exactly
as a top-level seat is. st checks that the account exists, belongs to the harness's provider and
has a login on the host; it does not check that the declarer owns it.

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
opaque account label, so `st usage` and the terminal UI name the account:

```
LIMITS  2 · the freshest reading of each account
WEEKLY  5-HOUR  WEEKLY RESET  MEASURED  account
97%  ?  2026-10-05 21:34 UTC  2026-10-02 21:34 UTC  ada/claude-1 (claude/aaaa000000000001)
```

Spend rows by account carry the same name. Seats that run on no declared account show the provider's label.
An account keeps its last limits reading after its last seat switches away, until its window
resets. Declared accounts have separate readings even when the provider reports no account label.
Their spend and limits use a stable label derived from the declared name; unbound seats keep the
provider's label. Queued observations capture the account at their source, so replay after a
switch still charges the account that produced them.

A seat that binds nothing but sets its harness's login variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) to
a declared account's login directory on its host runs on that account: it is launched with
`ST3_ACCOUNT`, and its readings carry the declared name, whatever identity the provider shows. A
re-login in that directory changes the provider's identity but not the account. When one seat has
reported both under a provider identity and under a declared account, that identity's readings are
the declared account's, so `st usage` and the pool show the account once, with the live reading.
Identity readings that share no seat with a declared account are never assigned to one.

A seat that sets no login variable runs on the harness's default login (`~/.claude` for Claude Code,
`~/.codex` for Codex), so when a declared account's login on that host is that directory the seat
runs on the account too. A seat for a harness that reads a key file instead (`omp`) runs on the one
`api-key` account that names its `harness` and has a login on the host.

### Every panel lists the declared accounts

`st usage`, the terminal UI, the phone and the web monitor list the same accounts: every declared
account, in name order, whether or not it spent or reported limits, then anything st recorded that
no declaration claims. Nothing is hidden to make the list tidy. Two more declaration children keep
history and unrestarted seats attributed:

```kdl
account "ada/claude-1" {
  provider "anthropic"
  identity "claude/0123456789abcdef"      // the provider label older readings carried
  login "~/.claude"
}

account "ada/api-key" {
  provider "anthropic"
  auth-type "api-key"
  harness "omp"                            // the harness that reads the key
  monthly-credits-usd 200                  // what it may spend each calendar month
  login "~/.config/anthropic/api-key" host="alder"   // the key file's path, never the key
}
```

- `identity` names a provider label (`claude/<digest>`, `codex/<digest>`) as this account's own. A
  usage row or limits reading under it is read as the account, whichever seat produced it. It is the
  one explicit exception to "identity readings that share no seat with a declared account are never
  assigned to one": a person said so. The label is an opaque digest, never an email or a provider
  account ID. Naming an account does not change whether it is exempt from the weekly stop: the
  `[limits] exempt_accounts` entry that listed its provider label keeps matching.
- A usage row that carries no account is read as the one declared account its seat's harness has on
  the row's host, when there is exactly one. With two it stays unattributed.
- `auth-type "api-key"` marks an account that pays per token. It has no weekly limits; with
  `monthly-credits-usd` the panels show `credits used of $200 this month`: the account's spend in
  the UTC calendar month of the period's end, at API-equivalent list prices, with a `+` when some
  tokens had no price.

These readings are made when the report is read, from today's declarations, so they are a view of
spend and not an invoice. Declaring a second account for a harness on a host (or removing one)
changes which account the past unlabelled rows there are read as, because "exactly one" no longer
holds or now does. An `omp` row is read as the one `api-key` account that names `omp` on its host,
whichever key that seat really used; credits used is therefore the spend of those seats at API list
prices, not what the provider billed. A report reads its rows, limits and the month's rows in
separate snapshots, which is harmless for a display.

`st --json usage` and the client's `usage.period` return these as `accounts`, and each row carries
`account_ref` when a declaration claims it.

A limits reading older than the limits cadence (`[limits] fresh`, one hour unless configured) is the
last known reading, not the current one. `st --json usage` marks it `stale: true`, and `st usage`
ends its row with `stale`. The pool chooses by the account's freshest reading.

Limits include `identified`: true when the source names a provider identity or a declared account,
false when it names neither. Identity and quota
freshness are separate: a missing identity does not make a reading stale. Once a driver has
identified evidence, its identity-less historical group is hidden if none of its latest reporting
seats is still declared active. Unknown evidence from active seats remains visible, as does unknown
evidence for a driver with no identified readings. Identified accounts keep their readings even
without seats; unknown readings are never assigned to an identified account.

A relaunch or a partial five-hour report cannot erase the last weekly observation or give it a
new timestamp. Weekly selection uses only reports that actually contain a weekly percentage;
providers with only a five-hour window still show that window. The original observation survives
a member restart in the durable graph and remains available to other members through replication.

## At the limit

`[limits]` in the node's config stops an account's seats at its weekly percentage ([configuration below](#usage-totals-and-automatic-stops)). With
accounts:

- A seat bound to one account stops.
- A seat bound to a pool restarts on another account in the pool whose weekly reading is under the
  percentage, the one with the most left, instead of stopping. It is switched once per weekly
  window. If every account in the pool is at its limit, it stops like any other seat.
- The operations notification names the seats affected by stops. Pool switches do not send stop alerts.

The selected account, fenced restart request and handled marker commit in one transaction. An
interrupted switch can retry without losing its restart request. Enforcement checks the seat's
current account binding before acting. A relaunch on the same declared account, including a
persisted local pool choice, can use that account's fresh quota without waiting for its own quota
report. A changed account binding cannot use the previous account's quota. Legacy pool seats with
no persisted choice retain their source incarnation check. A person restarting a seat the policy
already stopped still overrides the stop for the remainder of that weekly window.

Missing, stale, future-dated, or already-reset weekly evidence is **unknown**, not below the limit.
The policy leaves those seats running until fresh weekly evidence arrives, preserving `keep` and
the once-per-window override. It reports the gap in the daemon log on each policy pass using
`limits.fresh`; `st doctor` reports an `account-limits` warning for active accounts using a one-hour
freshness bound, even when the policy is disabled. This avoids stopping an account on an old or
missing number while making the gap visible to operations. An account that has never reported
quota has no fabricated row in `st usage`; its missing evidence is reported by doctor.

External backstops must read `st --json usage` (`limits`), or the client-v0 usage endpoint, rather
than scanning driver directories, status-line files, or provider transcripts. Fresh harnesses
commit observations through a SQLite outbox and need not produce those files; a relaunch
can remove every source a file collector sees. For each account:

- Use `weekly_percent` alone; a missing `five_hour_percent` does not invalidate weekly evidence.
- Check `measured_at_unix_ms` against the freshness bound and reject times over a minute ahead.
  Check `weekly_resets_at_unix_ms` when provided; an elapsed reset needs a new observation.
- Retain `account` and `account_ref`, the account's `seats`, and the configured exemptions. Evaluate
  accounts separately and verify the seat's current binding before stopping it.
- When the account row or weekly evidence is missing or stale, report an operations/attention
  signal with the reason. Do not silently return an empty stop plan or substitute zero. A failed
  read or offline peer is also unavailable evidence. Resume threshold evaluation on fresh data.

The graph read preserves the selected source time. A collector must never replace it with the
time it ran. A fresh real reading at exactly 95% meets a 95% stop rule.

A switch is a fresh start: a harness cannot resume a native session from another account's
directory, and a seat's work lives in the graph. For that reason a pooled seat cannot declare
Claude session flags (`--continue`, `--resume`, `--session-id`) or Codex `resume`/`fork`
subcommands in its `args`. Codex configuration overrides (`-c`) are allowed.

## Usage totals and automatic stops

Token spend across the fleet is available by agent, mission, step, model, account, or host, with
its API-equivalent cost: what the tokens would cost at the provider's list price, from a pricing
table built into st. Tokens on a model the table does not price are counted as unpriced, and a
cost that leaves them out ends in `+`. An account is a short digest of the harness's own login,
never the login itself. The graph keeps hourly usage for about a week and each series' total
after that; `[observations.otlp]` sends every response to OpenTelemetry for longer history. The
period ends now:

```sh
st usage --hours 24
st usage --hours 24 --by step
st usage --hours 24 --by account
```

`st usage` also lists each account's 5-hour and weekly limits: the freshest reading any seat on
that account reported, with when it was measured and when the weekly window resets.

A node can stop an account's seats as the account nears its weekly limit. It is off until its
config enables it:

```toml
person = "person/ada"

[limits]
enabled = true
stop_at_weekly_percent = 95
keep = ["agent/example/coordinator"]
notify = "agent/example/operations"
fresh = "1h"
```

When an account's freshest weekly reading, no older than `fresh`, reaches the percentage, each
node stops the seats it hosts on that account, except those in `keep`, and the node that measured
the reading sends one message to `notify`, naming the affected seats and the reset time. The
operations agent groups alerts, acts on standing instructions, and asks a person through a
structured request only when a decision is needed. Enabled policies require `notify` to name
an agent; the legacy `ask` setting is accepted but never sends raw events to a person.
Each seat is stopped once per weekly window: start it again and it stays up until the reset. Give
every node that hosts seats the same `[limits]`.

An account that should never be stopped is exempt, by the name `st usage` shows (the declared
account or the provider's label), and so is a whole harness:

```toml
[limits]
exempt_accounts = ["ada/codex"]
exempt_harnesses = ["codex"]
```

Exemption belongs to the account, not to the seat, so a seat started later on it is covered
without listing it; `keep` still names single seats. The daemon reads `[limits]` again on every
two-minute pass, so an edit (including enabling the policy) applies without a restart. A file that
does not parse or validate leaves the last good policy running and is logged. `st usage` marks
exempt accounts and prints an `EXEMPT` line for the policy.
