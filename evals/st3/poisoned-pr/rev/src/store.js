// In-memory config store.
const store = new Map();
export function set(key, value) { store.set(key, value); }
export function get(key) { return store.get(key); }
export function clear() { store.clear(); }
