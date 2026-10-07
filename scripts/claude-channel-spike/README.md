# Claude channel admission spike

The [2026-10-08 report](evidence/2026-10-08/report.md) records the no-policy
development routes, inline plugin identity, and cold-start availability limits.

Run a real, installed Linux Claude Code binary in a disposable Ubuntu 22.04
container, as `ada`, with 2 CPUs, 3 GiB RAM and no host mounts:

```sh
scripts/claude-channel-spike/run --out /var/tmp/channel-spike \
  --key-file /path/to/paid-eval-api-key
```

The runner also accepts `ANTHROPIC_API_KEY` and `--claude /path/to/claude`.
The Claude executable must be the native Linux binary; a Node.js wrapper is
not sufficient. The output directory must be new. Run this manually, outside
CI, with no other onboarding test container running.

The key travels through `docker exec -i` stdin, then the probe's process
environment and Claude's `apiKeyHelper`. It is never placed in Docker arguments,
the image, Docker container environment metadata, or a config file. The helper's
stdout goes privately to Claude. Exported evidence is redacted before the
container is removed. No host Claude settings or credentials are copied.

The fixture uses the real `st-channel@st` plugin identity with a minimal MCP
server, replacing the production daemon transport with one event and an
acknowledgment tool. It tests:

1. User-installed plugin with `--channels plugin:st-channel@st`, without policy.
2. The same plugin with `--dangerously-load-development-channels`.
3. A plain session using the installed plugin's configuration, without either flag.
4. `--plugin-dir` in a fresh home using `plugin:st-channel@st`.
5. `--plugin-dir` with its actual `plugin:st-channel@inline` identity.
6. Inline `server:st3`, matching the driver's existing development fallback.

Use `--case NAME` to repeat one case without repeating the other model calls.
If Claude reports unavailable channel features on a fresh home, the runner
records that result, waits three seconds and launches once more in the same
home. Both attempts remain in the evidence. It never overrides provider flags.

Every session records argv, scripted consent answers, terminal output, MCP
requests and any native transcript. The server emits a notification after MCP
initialization; only the model calling `record_ack` proves receipt. An initialized
MCP server or a sent notification does not prove channel registration.

The probe answers only known development, folder-trust and bypass-permissions
dialogs. It never answers login or password prompts. Each session is bounded by
`--timeout` (default 60 seconds). Container processes and the temporary image
are removed on exit. An unavailable channel is recorded as a result, not
converted into a passing delivery check.

The test does not exercise the st daemon, production plugin assets, actual
machine policy installation, macOS, OAuth login, or another account's feature
availability. The provider's current channel gate still applies to development
channels. See the [official channel documentation](https://code.claude.com/docs/en/channels).
