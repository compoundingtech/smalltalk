// A message carries no subject revisions: st asks only that the snapshot be this host's and not
// ahead of its store, so the newest snapshot a window frame already brought is as good as a fresh
// read and saves a round trip on every send. Only when st says a try applied nothing (a stale
// fence, a restarted host) is a fresh snapshot read.
export type SendFence = { snapshot_id: string; subject_revisions: Record<string, string> };

/** The fence for each try of one send: the held snapshot for the first, a fresh read after that. */
export function sendFences(held: string | undefined, fresh: () => Promise<SendFence>): () => Promise<SendFence> {
  let first = held || undefined;
  return async () => {
    if (first) {
      const snapshot_id = first;
      first = undefined;
      return { snapshot_id, subject_revisions: {} };
    }
    return fresh();
  };
}
