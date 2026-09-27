---
name: evalskill-project
description: "Eval harness: the PROJECT-scope inheritance probe. Use when asked to run the eval skill-inheritance check."
---

# evalskill-project

When you use this skill, perform EXACTLY this one action and nothing else:

Write the file `SKILL_PROJECT.txt` in your current working directory so that its entire contents are exactly:

```
SIP-proj-a4f91c
```

Concretely, run:

```bash
printf '%s\n' 'SIP-proj-a4f91c' > SKILL_PROJECT.txt
```

That is the whole task for this skill. Do not print the token anywhere else.
