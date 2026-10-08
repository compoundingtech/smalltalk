# Talking to agents

Messages are durable conversation. Missions track work and its result; [human requests](#ask-for-a-human-answer) track a decision that only a person can make. Use the worker from [getting started](getting-started.md), restarting it if you stopped it.

## From the terminal UI or phone

Run `stui`, open **Agents**, and select `garden/worker`. Click the conversation composer, type, and press **Enter** to send. In v0.3.4, **c** opens the composer from the conversation. **Ctrl+K** finds an agent or mission; **Ctrl+H** opens Home, and **Ctrl+Q** quits. The conversation shows your Smalltalk messages alongside the harness's transcript and tool calls.

On the phone, open **Agents**, select the worker, and use its composer. [Build and run the iOS app](ios-app.md) covers local installation first. A phone reads and acts through a paired member; it does not become a replica or run a seat. To begin pairing on your daemon machine:

```sh
st devices --as person/ada pair --full-control 'Garden phone'
```

Enter the returned pairing ID and code in the app with your paired-only gateway URL. Use [gateway setup](st3/client-v0/README.md#tailnet-carrier) and [the phone connection guide](../apps/ios/README.md#connect); never forward the privileged daemon socket. `--full-control` lets this trusted device send messages and use the work controls. An offline app keeps its last view; reconnect before sending an action.

Large native tool output is a preview, not the complete result. In stui, use
**Ctrl+Up/Down** to focus a tool or image and **Ctrl+Enter** to expand or collapse tool
output; **Ctrl+U**, outside the composer, loads or hides its native images inline
using the terminal's image renderer. **Ctrl+O** keeps its pane-zoom action. On the phone,
**Show all** loads the clipped result and **Show less** collapses it; **Load image**
displays the fetched image inline. Image media type and byte size
become exact after loading. Expanded native output and fetched native images stay
in memory only. This does not change mail attachment opening: stui still saves a
received mail image under its attachment directory and opens the machine's viewer.

## From the CLI

Capture the message ID so you can check it or keep the same thread:

```sh
hello_message=$(st conversations send agent/garden/worker --from person/ada \
  --subject 'Garden note' --body 'What did you finish in the first mission?')
st conversations status "$hello_message"
st conversations reply "$hello_message" --from person/ada \
  --body 'Please include the mission name in your answer.'
st conversations ls person/ada
```

`send` and `reply` safely recognize a retry with the same content. If a send times out, check its status or retry it; do not assume it failed. An agent's answer belongs in the conversation where you asked it.

Attach an existing PNG, JPEG, GIF, or WebP image (up to four, at most 10 MiB each):

```sh
printf 'Path to an image to share with the worker: '
read -r garden_image
st conversations send agent/garden/worker --from person/ada \
  --subject 'Garden image' --body 'Describe this image.' --attach "$garden_image"
```

Attachments are fetched by the member that reads or delivers the message. Ordinary workspace files do not replicate; put longer text in a graph document or a repository and send its reference. See [message examples](../examples/st3/SEND-A-MESSAGE-PROPERLY.md).

## Home shows what needs you

Home is your attention inbox: approvals, decisions, feedback requests, failures that need action, and information addressed to you. An agent working normally does not need a Home card. Follow progress under **Missions** and talk under **Agents**; clearing a card does not erase its history.

The CLI shows the same person-facing work:

```sh
st now
st attention ls --as person/ada
```

## Ask for a human answer

Agents use `st work ask` for a missing human decision, rather than leaving a question in their terminal. Requests can be:

| Type | Use it for |
| --- | --- |
| `decision` | One concrete action to accept, decline, or send back for changes. |
| `choice` | Two to five named alternatives; optionally allow a custom answer. |
| `feedback` | Free text about a page, draft, or result. |

For example, **inside an idle seat's shell**, save a reviewable decision and ask:

```sh
cat > publish-decision.json <<'EOF'
{
  "version": 1,
  "type": "decision",
  "question": "Publish the garden README?",
  "why_person": "Ada decides when this draft is ready for readers.",
  "subjects": [{"kind": "document", "label": "Garden README", "ref": "README.md"}],
  "answers": [
    {"id": "publish", "label": "Publish", "outcome": "accept", "consequence": "The agent can publish the reviewed draft."},
    {"id": "keep", "label": "Keep draft", "outcome": "decline", "consequence": "The draft stays private."},
    {"id": "revise", "label": "Request changes", "outcome": "request_changes", "consequence": "The agent revises it using Ada's feedback."}
  ]
}
EOF
st work ask --for person/ada --title 'Review the garden README' \
  --new-run garden/readme-decision --request publish-decision.json \
  --idempotency-key garden-readme-decision --as "$ST_AGENT"
```

Use `--step` for a currently claimed step instead of `--new-run`, as [missions in practice](missions-in-practice.md#ask-for-a-decision-that-comes-up-during-work) shows. Name why the person is needed and the exact subject/revision they are reviewing. The agent finishes all work it can do independently before asking.

For feedback, the agent can instead write:

```sh
cat > readme-feedback.json <<'EOF'
{
  "version": 1,
  "type": "feedback",
  "question": "Read README.md as a new gardener. What is unclear?",
  "why_person": "A reader's experience cannot be inferred from the link checks.",
  "subjects": [{"kind": "document", "label": "Garden README", "ref": "README.md"}]
}
EOF
st work ask --for person/ada --title 'Read the garden README' \
  --new-run garden/readme-feedback --request readme-feedback.json \
  --idempotency-key garden-readme-feedback --as "$ST_AGENT"
```

Ada answers in Home, or uses the exact person-step ID from the attention inbox:

```sh
st attention ls --as person/ada
printf 'Person step-run ID to answer: '
read -r person_step
st work show "$person_step"
```

For the decision above:

```sh
st work done "$person_step" --as person/ada --answer publish
```

For a feedback request, select its own step ID and provide text instead:

```sh
st work done "$person_step" --as person/ada --text 'Explain when visitors can take produce.'
```

Choice answers use their named ID in the same way. Feedback, a custom choice, or requested changes need text. The answer is durable: a step-owned ask resumes its worker; an idle-seat ask delivers the answer to its requester as a message.

## Read and find conversations

Messages are for conversation, not for handing out work:

```sh
st conversations send agent/example/worker --from person/ada \
  --subject "Hello" --body "Reply with one short sentence."
st conversations ls person/ada
st conversations read MESSAGE --as person/ada
st conversations thread MESSAGE
st conversations archive MESSAGE --as person/ada
st conversations search "release date" --agent agent/example/worker --since 2026-10-01T00:00:00Z
```

Running the same send or reply again never sends it twice. Without `--idempotency-key`, st names
the message by its sender, recipient (or the message it answers), title, body, tags, attachments
and the hour, so a repeat within the hour or the next reports the message already sent
(`already_sent` with `--json`) and sends nothing; the same words a few hours later are a new
message. An explicit `--idempotency-key` names one message at any hour, and reusing it with
different words is refused.

If a send goes unanswered, st retries once with the same key. If it is still unconfirmed, the
message may have landed: the error prints its key and the command that tells whether it did,
`st conversations status --idempotency-key KEY`. Running the same command again is safe.

`st conversations sessions` lists harness sessions and `st conversations timeline SESSION`
shows one conversation as stui shows it: messages, Smalltalk, and tool calls folded to a line
or two. `--raw` prints every stored entry instead (message boundaries, tool input and output in
full), and `--json` prints the page.

`conversations search` searches the authenticated person's sent and received messages and
the normalized transcripts they can view. It returns conversation and entry IDs with short
excerpts, newest first. Use `--cursor` for older matches, or `--json` for the typed client
response. The response dates its index and reports incomplete sources, including retained
history limits and unavailable hosts. See [conversation search](st3/conversation-search.md)
for freshness, costs, and the embedding API.
