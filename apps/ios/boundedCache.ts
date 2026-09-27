export function rememberBounded<K, V>(cache: Map<K, V>, key: K, value: V, limit: number): void {
  if (limit < 1) throw new Error('cache limit must be positive');
  cache.delete(key);
  cache.set(key, value);
  while (cache.size > limit) {
    const oldest = cache.keys().next();
    if (oldest.done) break;
    cache.delete(oldest.value);
  }
}
