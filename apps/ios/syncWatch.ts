import type { SyncNotice } from '../../clients/typescript/st3-client';

/** `undefined`: no notice has been read; `null`: read, and this host is level with its peers. */
export type SyncRead = SyncNotice | null | undefined;

/**
 * Read the page `sync` notice now and then every `intervalMs`, publishing what each read found. The
 * alerts arrive over the live socket, whose frames carry no notice, so this is a small page read.
 * A read that fails publishes `undefined`: not knowing is not the same as being level.
 */
export function watchSync(read: () => Promise<SyncNotice | null>, publish: (value: SyncRead) => void, intervalMs = 60_000): () => void {
  let active = true;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const once = async () => {
    let value: SyncRead;
    try { value = await read(); } catch { value = undefined; }
    if (!active) return;
    publish(value);
    timer = setTimeout(() => void once(), intervalMs);
  };
  void once();
  return () => { active = false; if (timer !== undefined) clearTimeout(timer); };
}
