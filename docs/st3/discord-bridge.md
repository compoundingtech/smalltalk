# One-channel Discord bridge

The image and external-account update requires the native
`POST /v1/client/adapter/import` endpoint and an enrolled program seat. This source
update is being verified; the existing live text pilot remains on its previous
code until native deployment and route enrollment are ready.

Outbound replies in the updated bridge use an enrolled native delivery wait with a
bounded local graph cursor, rather than repeated complete mailbox scans. Accepted
pages are durably queued before the cursor advances; restart replays stable IDs.
Changing hosts or restoring a graph behind the retained frontier requires explicit
reconciliation. Discord intake still polls every five seconds. This interim wait
uses existing graph notifications; the planned graph-watch engine can replace that
notification source without changing reply correlation or provider receipts.

While the native update is pending, `--legacy-text` runs the text pilot using its
existing provisional identities and mailbox observation. Repeated `--allow-user`
flags explicitly enroll additional numeric accounts in that mode. Each account
gets its own `person/discord-<id>` sender and reply correlation; enrollment does
not change the primary route binding or erase delivery checkpoints. Bots and
webhooks remain excluded. Enrollment applies to new channel messages, without
replaying messages already skipped. Example: `--legacy-text --allow-user 606`.
Additional accounts in external-identity mode require native multi-route enrollment
and are refused by the current command. Images remain disabled in legacy mode.

The bridge is a normal-user Python process, run in a terminal or an st-supervised
program seat. It polls one private
Discord text channel every five seconds and routes one allowlisted Discord user
to one explicitly configured st agent. It needs Python's standard library and the
existing local st Unix socket. There is no package installation or root service setup.

Use the private route prepared for the connectivity probe at
`~/.local/state/st3/discord-probe/trial.json`. It contains application, server,
channel and user IDs, never a token. Example values in the probe documentation
are invented. The token's application, channel's server/type and bot account are
checked before importing messages.

From your own terminal on the machine running st:

```sh
python3 -I scripts/st3-discord-bridge run --agent agent/example/discord-test
```

Replace the invented agent subject with the intended existing agent. Paste the
bot token at the hidden local prompt. On a first launch, wait for
`Bridge ready. Send a new text message in the Discord test channel.` Then send
an ordinary text message in Discord. Keep the terminal open while chatting.
An agent must reply using st's normal reply operation to preserve the message link.
Its answer appears as a Discord reply to the corresponding message, with mentions
disabled. You do not need to type a special exact test phrase.

Ctrl-C stops the bridge. Restart with the same command and token prompt; the
private delivery state is retained. The first launch skips existing channel
history. Restarts resume from the retained message boundary, including messages
sent while the bridge was stopped, subject to the bounded catch-up limit.

For an overnight run without setting up a secret gateway, you can supply a
private dotenv file explicitly. Create it locally, outside the checkout:

```sh
install -d -m 700 ~/.config/st3-discord
(umask 077; nano ~/.config/st3-discord/bot.env)
chmod 600 ~/.config/st3-discord/bot.env
```

In the editor, put one line, replacing the placeholder with the bot token:
`DISCORD_BOT_TOKEN=<paste-token-here>`. Do not put the token into a shell command,
st message or repository. The loader accepts that one entry, optionally quoted,
plus blank lines and full-line comments; it never sources shell code or exports
the value into the environment. It refuses shared files, symlinks, hardlinks,
non-regular files and files owned by another user. The token remains on disk until
you remove the file yourself.

Stop the current bridge before launching its replacement. To run it under st,
adapt the invented paths, placement and subjects in
[`examples/st3/discord-bridge.kdl`](../../examples/st3/discord-bridge.kdl), then
preview and apply that declaration:

```sh
st apply /path/to/discord-bridge.kdl --dry-run --as agent/example/operator
st apply /path/to/discord-bridge.kdl --as agent/example/operator
st agents show agent/example/discord-bridge
st terminals peek agent/example/discord-bridge
```

The top-level program seat uses `argv` to run Python directly in st's managed PTY,
without a model harness. It remains independent of finite verification missions.
`restart "always"` restarts an exit, subject to st's crash-loop guard. To stop it
durably, use `st agents stop agent/example/discord-bridge --as agent/example/operator`;
Ctrl-C alone allows the supervisor to restart it. To resume, use the corresponding
`st agents start` command. `st terminals attach agent/example/discord-bridge`
opens its terminal; Ctrl-\\ detaches. The runtime keeps running across SSH
disconnects. Reboot startup depends on the existing st daemon starting on that host.

The token path is recorded in the declaration; the token itself is read only by
the bridge from the private file. `--token-file` works for `run` and `resolve`;
`status` never reads a token. Secret-gateway setup is not a prerequisite for this
program seat or the provider identity migration.

The recorded st sender is derived from the Discord user ID, for example
`person/discord-404`. It has no link to an existing native st person. Server
ownership, Discord display names and mentions do not change that identity or the
configured destination. The body carries server, channel and message provenance;
the private state records the route and Discord/st message mappings. This version
uses the same trusted local message API as `st conversations`; its route allowlist
is enforced by the foreground bridge. It does not create a separately restricted
st credential or expose the Unix socket to Discord.

Discord replies to imported messages or exported agent replies preserve their
st reply links. A reply to an unmapped historical message starts a new st
conversation. Unrelated st messages, another agent's messages, other Discord
authors, bot/webhook messages and other channels are not forwarded. One bot
installation and one state file should own the route.

This version forwards text. Images, files, embeds, stickers, DMs, multiple agent
destinations and Discord thread creation are deferred. Visible attachments are
omitted with a note when the message also has text. Attachment-only messages are
skipped with a terminal notice. Edits and deletes after import do not replace or
re-execute an agent instruction. A deletion cannot retract an already imported
instruction; an outgoing reply to a deleted source is refused and retained.
If an allowlisted message has no readable text or visible attachment, the bridge
holds its cursor and asks you to check Message Content Intent.

With the default hidden prompt, the token stays in process memory and is never
saved. With `--token-file`, the operator explicitly supplies their own local
credential file. Neither mode prints the token, passes it to st or places it in
command arguments or the environment. Hidden input must be available when no
file is supplied. Redirects and proxy
environment settings are ignored for the authenticated Discord connection.
Provider errors and unexpected exceptions are sanitized. Text containing the
current bot credential is rejected before import/export.

The default SQLite state lives in the private directory
`~/.local/state/st3/discord-bridge/`, bound to the exact route and agent. The state
contains conversation text and IDs, with no token; protect it like a conversation
history. Its directory is mode 0700 and files are mode 0600. A process lock prevents
two launches sharing that file. Do not delete it to fix a delivery error: doing so
loses correlation and delivery evidence. `--route`, `--state` and `--socket` allow
explicit local paths; the socket must be owned by your user and not world-accessible.

Accepted messages remain queued if st is temporarily unavailable. Once accepted
by st, its normal durable mailbox holds them for an offline agent. At most 100
messages wait for agent replies; at capacity, the bridge holds its Discord cursor
and leaves new messages in channel history. Catch-up is bounded to 1000 messages
and retained conversation/reply records to 10000 each. Exceeding a limit stops or
holds intake visibly, preserving the boundary rather than skipping a backlog.
Inbound text is limited to 3000 UTF-8 bytes. Longer agent replies are split into
Discord-sized messages with a stable nonce per part. Rate-limit headers and
`retry_after` delays govern retries; transient failures back off to 60 seconds.
Authentication/permission refusals stop the process.

Native sends retry the same delivery key and immutable body after an unknown
outcome. Discord sends are durably checkpointed before posting. On an unknown
outcome, the bridge searches recent messages for a matching bot author, nonce,
text and reference. If it cannot prove delivery, it holds outgoing sends instead
of blindly reposting, even after Discord's short nonce-deduplication window.

For an unresolved send, stop with Ctrl-C, inspect the posted Discord message and
copy its ID. `status` prints retained IDs and delivery states without a token.
`resolve` authenticates a human-selected receipt against the queued reply's bot
author, channel, text, reference and nonce when present:

```sh
python3 -I scripts/st3-discord-bridge status --agent agent/example/discord-test
python3 -I scripts/st3-discord-bridge resolve --agent agent/example/discord-test \
  --st-message message/0123456789abcdef --discord-message 505 --part 0
```

Then restart `run`. If no matching message exists, retain the held state for
inspection. This version deliberately has no automatic or blind retry command
for an uncertain Discord send.

Run focused checks:

```sh
python3 -I scripts/st3-discord-bridge-test
ST3_BRIDGE_NATIVE_TEST_BIN=/path/to/st3 python3 -I scripts/st3-discord-bridge-test
python3 -I scripts/st3-discord-probe-test
```

The native check creates and cleans up an isolated daemon with invented identities
and its own temporary config, sockets and state. It proves sender attribution,
idempotent native sends, mailbox pagination and reply links using the installed
binary. Other checks cover the bridge round trip with fake Discord responses,
author restrictions, burst pagination, restart and timeout recovery, rate delays,
optional nonce handling, hidden input and credential redaction. They are not a
live Discord/agent round-trip proof. That proof requires the operator's local
token entry, an actual agent reply, and a fresh exchange after restarting.

New imports use `external/discord/user/<id>` through the route-scoped native import
endpoint. Existing `person/discord-<id>` imports retain their original identity;
the bridge reads both mailboxes and preserves their reply mappings.
The program-seat declaration must bind the exact source and target through
`st3.adapter.source` and `st3.adapter.target`, as shown in the example declaration.
The authenticated adapter uploads as itself and cannot impersonate a native person.
Native person-to-provider-account edges can represent verified account links
without collapsing the provider sender into a native person.

Images use native content-addressed blobs and Discord multipart uploads. PNG,
JPEG, GIF and WebP are supported, with up to four images and 10 MiB total per
message. Image-only messages are accepted. Downloads use anonymous HTTPS requests
to the configured channel's Discord CDN paths, with no redirects; bytes, type,
size and delivery hashes are checked. Non-image files and embedded URLs are not
downloaded. Outgoing images appear on the first part of a split text reply.
An uncertain image delivery is held until a matching receipt proves its hashes;
restart does not blindly resend it.

Discord API references: [messages, replies and nonce deduplication](https://docs.discord.com/developers/resources/message),
[rate-limit headers and retry delays](https://docs.discord.com/developers/topics/rate-limits),
and [Message Content Intent across APIs](https://docs.discord.com/developers/events/gateway#message-content-intent).
