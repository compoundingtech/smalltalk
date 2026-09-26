// A tiny command-dispatch core for the `ledger` service.
//
// register() is LAST-WINS BY DESIGN: registering the same command twice just
// overwrites the handler with an identical one — so re-processing a work-item is
// harmless (no duplicate-registration corruption). That is the durability
// property this service leans on: an at-least-once pipeline makes each step
// idempotent, so a retried/redone step never breaks the artifact.
const handlers = new Map();

export function register(command, handler) {
  handlers.set(command, handler); // last-wins: a redo re-registers the same handler harmlessly
}

export function dispatch(command, input) {
  const handler = handlers.get(command);
  if (!handler) throw new Error(`no handler registered for command: ${command}`);
  return handler(input);
}

// The sorted list of registered command names (no duplicates — a Map has unique keys).
export function registered() {
  return [...handlers.keys()].sort();
}

// ── work-item handlers ───────────────────────────────────────────────────────
// One handler is added here per work-item (see items.json), in order. Each is a
// small pure function registered under its command name, e.g.
//
//     register("upper", (input) => String(input).toUpperCase());
//
// (none yet — the batch adds item-1..item-4)
