# Harness integrations

`st setup` finds Claude Code, Codex, Omp, Pi and OpenCode on the daemon's login PATH.
It offers one installation plan listing every harness it found. Choosing a harness for
onboarding is a separate choice: choosing Codex still offers the Claude integration when
Claude is also installed.

| Harness | Installation |
| --- | --- |
| Claude Code | Bundled st skill and the user-owned `st-channel@st` plugin |
| Codex | Bundled st skill; managed seats use st's native app-server driver |
| Omp | Bundled st skill and the verified managed channel extension |
| Pi | Bundled st skill and the verified managed channel extension |
| OpenCode | Bundled st skill; managed seats use st's native HTTP driver |

The Claude skill goes in `~/.claude/skills/st/SKILL.md`, or under
`CLAUDE_CONFIG_DIR/skills` when that directory is configured. The other four harnesses
share `~/.agents/skills/st/SKILL.md`. The skill applies only in a session st started.
Pi and Omp load the immutable extension set under st's state directory when their managed
seats start; setup does not add a second global extension with duplicate callbacks.

Claude plugin assets remain installed at user scope, with user enablement disabled. Managed
seats select the plugin in their own settings. An ordinary Claude session that explicitly
enables it gets a healthy idle MCP server with no seat tools or channel notifications.
Setup never installs administrator channel policy. Managed Claude seats use existing policy
when available, or development channel admission otherwise.

Scripts can accept or decline the complete plan with `--integrations true|false`.
`--claude-channel false` excludes only the user plugin; the consent plan still includes the
Claude skill and every other detected harness. Claude remains available through its inline
managed channel. `--harness none` skips harness setup and onboarding entirely.

```sh
st setup --harness codex --integrations true --claude-channel false
st doctor --json
```

Doctor reports `integration/claude`, `integration/codex`, `integration/omp`,
`integration/pi` and `integration/opencode` for harnesses found by the latest setup. It checks exact installed
skill bytes and managed extension assets. Explicit setup verifies Claude user plugin
registration and exercises the no-subject MCP initialize, tools-list and ping path, then
records evidence without saving the login environment or provider credentials. Doctor reads
that evidence without launching a shell, provider CLI, credential helper or MCP child.
Binary or registration changes make the recorded native evidence stale; rerun setup to refresh it.
Missing optional installations warn; damaged assets fail. Rerun `st setup` to repair them.
These installation checks are separate from provider login, organization restrictions and
the live seat delivery observations already in doctor.

## Claude's native first launch

On a fresh account, finish Claude's native welcome, account or API-key confirmation and
permission-bypass consent in a normal terminal under the same user and profile as st.
See [Getting started](../getting-started.md). An API key in the environment, a successful
print-mode request or `claude auth status` alone does not establish that interactive setup
is complete. Claude's [authentication documentation](https://code.claude.com/docs/en/authentication#authentication-precedence)
explains its environment-key approval and supported private key helper.

If the expert is already waiting at a native setup screen, finish that setup and restart
its existing seat:

```sh
st agents restart agent/st/expert --as person/ada
```

Use your configured person name in place of `ada`. Restarting preserves the onboarding run
and queued messages. st does not answer native account questions or write fabricated
welcome-completion preferences.
