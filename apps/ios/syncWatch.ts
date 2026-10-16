import type { SyncNotice } from '../../clients/typescript/st3-client';
import { syncLine, syncView } from '@smalltalk/st3-views';

/** `undefined`: no notice has been read; `null`: read, and this host is level with its peers. */
export type SyncRead = SyncNotice | null | undefined;

/** A read that has not answered by now counts as failed, so one hung request cannot freeze the banner. */
export const READ_TIMEOUT_MS = 15_000;

const says = (value: SyncRead) => syncLine(syncView(value)) !== undefined;

/**
 * Read the page `sync` notice now and then every `intervalMs`, publishing what each read found. The
 * alerts arrive over the live socket, whose frames carry no notice, so this is a small page read.
 * A read that fails or times out never says "level": it publishes `undefined`, except that a notice
 * that said this host is behind stays up, marked `stale`, until a read answers. Not knowing is not the
 * same as being level, and a failed check is not the same as being caught up.
 */
export function watchSync(
  read: () => Promise<SyncNotice | null>,
  publish: (value: SyncRead, stale: boolean) => void,
  intervalMs = 60_000,
  timeoutMs = READ_TIMEOUT_MS,
): () => void {
  let active = true;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let last: SyncRead;
  const once = async () => {
    let value: SyncRead;
    let stale = false;
    let deadline: ReturnType<typeof setTimeout> | undefined;
    try {
      value = await Promise.race([read(), new Promise<never>((_, reject) => { deadline = setTimeout(() => reject(new Error('timed out')), timeoutMs); })]);
    } catch {
      value = says(last) ? last : undefined;
      stale = value !== undefined;
    } finally {
      if (deadline !== undefined) clearTimeout(deadline);
    }
    if (!active) return;
    last = value;
    publish(value, stale);
    timer = setTimeout(() => void once(), intervalMs);
  };
  void once();
  return () => { active = false; if (timer !== undefined) clearTimeout(timer); };
}

/** The banner line while this host is behind, or undefined; a notice that could not be refreshed says so. */
export function syncBannerText(sync: SyncRead, stale: boolean): string | undefined {
  const line = syncLine(syncView(sync));
  return line && stale ? `${line} Not refreshed: the last check failed.` : line;
}
