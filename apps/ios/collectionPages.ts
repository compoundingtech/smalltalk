import type { Page, Snapshot } from '../../clients/typescript/st3-client';

function isSnapshotChurn(error: unknown): boolean {
  return typeof error === 'object' && error !== null && 'response' in error
    && typeof error.response === 'object' && error.response !== null
    && 'code' in error.response && (error.response.code === 'page-cursor-expired' || error.response.code === 'cursor-gap');
}

export type CollectionResult = { pages: Array<{ value: Page; snapshot: Snapshot }>; truncated: boolean };

/**
 * Every row of every page once, by its id. A row that moved while the pages were read can be on
 * two of them; the later copy is the newer, and it keeps the earlier place.
 */
export function mergedRows<T extends { id?: string }>(rows: T[]): T[] {
  const places = new Map<string, number>();
  const merged: T[] = [];
  for (const row of rows) {
    const id = row.id;
    const place = id === undefined ? undefined : places.get(id);
    if (place === undefined) {
      if (id !== undefined) places.set(id, merged.length);
      merged.push(row);
    } else merged[place] = row;
  }
  return merged;
}

export async function listCollectionPages(
  list: (options: { limit: number; cursor?: string }) => Promise<{ value: Page; snapshot: Snapshot }>,
  limit: number,
  maxPages = 5,
): Promise<CollectionResult> {
  if (!Number.isInteger(limit) || limit < 1 || !Number.isInteger(maxPages) || maxPages < 1) throw new Error('Invalid collection page bounds.');
  for (let attempt = 0; attempt < 3; attempt++) {
    const pages: CollectionResult['pages'] = [];
    const seen = new Set<string>();
    let cursor: string | undefined;
    try {
      for (let number = 0; number < maxPages; number++) {
        const result = await list({ limit, cursor });
        pages.push(result);
        if (!result.value.page.has_more) return { pages, truncated: false };
        const next = result.value.page.next_cursor;
        if (!next || seen.has(next)) throw new Error('Collection pagination did not advance.');
        seen.add(next);
        cursor = next;
      }
      return { pages, truncated: true };
    } catch (error) {
      if (!isSnapshotChurn(error) || attempt === 2) throw error;
      await new Promise(resolve => setTimeout(resolve, 100 * (attempt + 1)));
    }
  }
  throw new Error('Collection pagination retries exhausted.');
}
