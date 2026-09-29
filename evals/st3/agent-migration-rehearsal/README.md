# Agent migration rehearsal

This eval starts one isolated Codex agent. It does not read, stop, or change a live st2 agent.

The mission starts one isolated agent with no startup prompt. It checks four things: the runtime writes no `.st3` directory, a `render` copy writes the exact migration document to `HOST.md`, the agent completes normal assigned work, and cleanup stops the agent.

The eval does not declare a physical host. An eval cannot replace selected host metadata on its production daemon.

Run this eval before the first live agent migration. A live migration still needs separate human authorization.
