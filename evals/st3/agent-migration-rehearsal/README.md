# Agent migration rehearsal

This eval starts one isolated Codex agent. It does not read, stop, or change a live st2 agent.

The mission tests an omitted harness prompt, the generated `.st3/boot.md` file, an exact migration document, normal assigned work, and cleanup.

The eval does not declare a physical host. An eval cannot replace selected host metadata on its production daemon.

Run this eval before the first live agent migration. A live migration still needs separate human authorization.
