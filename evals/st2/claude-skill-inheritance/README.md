# Claude Skill Inheritance eval for st2

This eval tests the union of project and plugin skills in one native Claude Sonnet seat.

The task does not contain either secret token. Each loaded skill writes its own token to a sentinel file.

The plugin uses a session-only relative `--plugin-dir`. The isolation judge checks the personal Claude scope.

Run it with `st2 eval ./evals/st2/claude-skill-inheritance`.

The source fixture came from `compoundingtech/evals` commit `3db48ab56d40ce27dfd94f89d2db9b692d93836a`.
