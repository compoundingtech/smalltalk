# Discord connectivity probe

Run `scripts/st3-discord-probe` as a normal user with Python's standard library.
It needs no installation, sudo, system service, or sekrets gateway.

The combined `test` checks the token's application and configured text channel,
posts one labelled message, then asks you to type `st probe hello` in Discord.
Then press Enter in your terminal without typing anything. It verifies that message came from the configured
Discord user. It opens no Gateway connection and routes nothing to st agents.
A passing REST test does not prove thread permissions, Gateway delivery, or bridge recovery.

Keep the route outside git in a private file owned by your user (mode 0600).
The default is `~/.local/state/st3/discord-probe/trial.json`. Example values are invented:

```json
{
  "application_id": "101",
  "guild_id": "202",
  "channel_id": "303",
  "user_id": "404",
  "nonce": "inventedprobe20261006"
}
```

From your own interactive terminal:

```sh
python3 -I scripts/st3-discord-probe test --route /path/to/private/trial.json
```

Paste the bot token directly from your password manager at the hidden local prompt.
It stays in this process; it is never saved, printed, put in command arguments,
passed through st, or loaded into the agent's environment. The program stops if
hidden input is unavailable. No additional host configuration is needed.

Individual `check`, `send`, and `verify` operations also accept `--token-stdin`
for a direct password-manager pipe. Do not substitute the token into a shell command.
Combined `test` uses an interactive prompt so you can confirm your Discord reply.

If the bot already posted successfully but the read check failed, send
`st probe hello` as a message in that Discord channel, then run
`python3 -I scripts/st3-discord-probe verify`. This checks the reply without
posting another test message and asks for the token again at the hidden prompt.

The configured nonce is a non-secret probe identifier. Discord only deduplicates it
within a short window. Inspect the channel before retrying an uncertain send; this
probe never automatically retries a POST, rate limit or authentication refusal.
Mention parsing is disabled. Redirects are refused and response sizes are bounded.

`verify` checks the latest 20 messages and rejects another author, bot or webhook
messages, and absent content. Failed checks distinguish an empty message listing,
a missing user, empty text, and text that does not match the marker, without printing
message bodies. If user text is empty, check Developer Portal > your application >
Bot > Privileged Gateway Intents > Message Content Intent. Enable it and save;
[Discord applies this setting to HTTP reads too](https://docs.discord.com/developers/events/gateway#message-content-intent).
DMs, forums, threads,
agent attribution and the persistent bridge are outside this connectivity test.

Run token-free checks with `python3 scripts/st3-discord-probe-test`.
They cover normal-user configuration, the combined test, hidden input, wrong route
refusal, author checks, credential redaction, uncertain sends and redirect refusal.

Discord references: [bot application information](https://docs.discord.com/developers/topics/oauth2#get-current-bot-application-information),
[channel API](https://docs.discord.com/developers/resources/channel), and
[message API](https://docs.discord.com/developers/resources/message).
