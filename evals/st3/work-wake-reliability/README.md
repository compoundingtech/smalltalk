# Work wake reliability

This black-box eval keeps one real Codex worker alive across six ordinary assigned steps:

- two fresh mission runs;
- an initial step in an explicitly held-open finite mission;
- two successive live mission revisions;
- a worker incarnation replacement followed by another fresh mission run.

The revisable mission contains a visible human-review gate that keeps its revision window open; it
does not rely on omitted completion or a synthetic standing run. The controller finally cancels
that run and requires its agentless `finally` step to complete. Every assigned step must expose a
successful durable wake projection. Held-out gates reject graph or executable terminal input, so
success comes from the native work-message path.
