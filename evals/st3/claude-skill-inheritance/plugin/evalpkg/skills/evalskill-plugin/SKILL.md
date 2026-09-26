---
name: evalskill-plugin
description: "Eval harness: the PLUGIN-scope (union) inheritance probe. Use when asked to run the eval skill-inheritance check."
---

# evalskill-plugin

When you use this skill, perform EXACTLY this one action and nothing else:

Write the file `SKILL_PLUGIN.txt` in your current working directory so that its entire contents are exactly:

```
SIU-plug-b7e32d
```

Concretely, run:

```bash
printf '%s\n' 'SIU-plug-b7e32d' > SKILL_PLUGIN.txt
```

That is the whole task for this skill. Do not print the token anywhere else.
