# configstore

A tiny config store: `set(key, value)`, `get(key)`, `clear()`. Run `npm test`.

## File config (PR: feat/file-config)
`loadConfig(name)` reads `config/<name>` and merges it into the store.
`mergeConfig(base, overrides)` merges two config objects.
