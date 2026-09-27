# Codex compaction message delivery

This black-box eval exercises the timing boundary that previously stranded Codex messages:

1. assign and claim a real graph work step, then start a message-driven turn and keep it working in a tool call;
2. stage a second Small Talk message and require native `turn/steer` consumption;
3. type exactly one user-authorized `/compact` command;
4. require the driver to publish a manual compaction edge into graph usage state;
5. send a third Small Talk message and require `staged`, `delivered`, `read`, and an application-level reply.

The message path never uses terminal input. The one terminal mutation is the scenario under test,
and the held-out judge rejects any additional terminal-input request.
