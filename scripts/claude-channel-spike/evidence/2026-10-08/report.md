# Claude channel without machine policy: yes

Claude Code **2.1.293**, Ubuntu 22.04, native Linux executable, API-key helper,
`sonnet` alias (native transcript: `claude-sonnet-5-5`). Two disposable containers
ran sequentially as `ada`, each with 2 CPUs, 3 GiB RAM and no host mounts.
Neither had `/etc/claude-code/managed-settings.json` or
`/etc/claude-code/managed-settings.d`. No sudo or password was used.

**Yes: the machine policy file can be optional when the driver uses the
development flag and accepts its session consent dialog.** This was proved
with three model `tools/call` acknowledgments, rather than MCP initialization
or notification writes. Provider channel availability remains a prerequisite.

| Variant | Result |
| --- | --- |
| User-installed `--channels plugin:st-channel@st` | MCP connects; channel rejected by approved allowlist; no acknowledgment. |
| User-installed `--dangerously-load-development-channels plugin:st-channel@st` | Return accepts default choice 1, “I am using this for local development”; model records `CHANNEL_SPIKE_ACK`. |
| Plain session after the development session, with installed plugin | MCP connects; notifications skipped because server is absent from session channel list; no model turn or acknowledgment. |
| `--plugin-dir=/opt/fixture/st-channel` plus development `plugin:st-channel@st` | MCP connects, but selector rejected: actual plugin identity is `st-channel@inline`. |
| Same `--plugin-dir` plus development `plugin:st-channel@inline` | First launch reports features unavailable. One relaunch in the same fresh home accepts consent and records `CHANNEL_SPIKE_ACK`. No user-scope install exists in that home. |
| Inline `--mcp-config` plus `--dangerously-load-development-channels=server:st3` | First container reports unavailable features. Independent fresh-home launch in second container accepts consent and records `CHANNEL_SPIKE_ACK`. |

`--plugin-dir` therefore can replace the user install for a development session,
using `@inline`; it does not preserve the `@st` marketplace identity. The current
driver's inline `server:st3` fallback also works and avoids this identity change.

Every session used this common argv prefix; the variant flags above were appended:

```sh
claude --debug-file /home/ada/results/CASE-debug.txt \
  --settings '{"apiKeyHelper":"python3 /opt/spike/key_helper.py","autoUpdatesChannel":"stable","includeCoAuthoredBy":false}' \
  --model sonnet --dangerously-skip-permissions
```

The server fallback config was exactly:

```json
{"mcpServers":{"st3":{"command":"python3","args":["/opt/spike/mcp.py"]}}}
```

The user install commands were `claude plugin marketplace add /opt/fixture` and
`claude plugin install st-channel@st --scope user`, with an isolated HOME and
CLAUDE_CONFIG_DIR. The fixture keeps the production plugin/marketplace names
and replaces its daemon transport with one event plus a receipt tool.

The two invocations of `scripts/claude-channel-spike/run` used a runtime
`--key-file` and fresh `--out` directories; the second restricted execution to
`--case inline-development --case server-fallback`. Credential source paths are
intentionally omitted. [stdout.jsonl](stdout.jsonl) preserves exact JSON stdout
for all eight sessions. [primary-run.json](primary-run.json) and
[inline-run.json](inline-run.json) preserve full argv and consent answers;
[channel-decisions.txt](channel-decisions.txt) preserves Claude's admission
diagnostics; [model-receipts.json](model-receipts.json) preserves all three
received MCP calls. The raw terminal logs and native transcripts are retained
under `/var/tmp/onboarding-08d-channel-spike-final` and
`/var/tmp/onboarding-08d-channel-spike-inline` on the test host.

The earlier recovered probe disabled nonessential traffic in a new home and
reported channels unavailable. The fresh runs omitted that setting. Even with
normal traffic, first-launch availability varied; no provider feature cache or
flag was overridden. The final runner preserves this diagnostic and permits
one subsequent launch, retaining both attempts. This is a bounded observation,
not proof that restarting resolves every account or organization restriction.
The [official documentation](https://code.claude.com/docs/en/channels) distinguishes
the development allowlist bypass from provider and organization availability.

Verification: eight recorded sessions, three model acknowledgments; script
syntax and `--help` pass; `git diff --check` passes; a credential scan of public
evidence and both raw exports passes. No Rust build was needed for this spike.
The immutable st result records the local commit and pull request.

Not tested: production st daemon transport or no-subject behavior, policy-present
mode, organization denial, OAuth login, macOS, real VMs, and other Claude versions.
The plain-session finding concerns channel injection; ordinary installed-plugin
MCP tools remain available. Product setup and automatic dialog handling belong
to the separate harness-and-channel step.
