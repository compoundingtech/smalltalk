import type { SyncNotice, SyncPeer } from '@smalltalk/st3-client';

// The optional `sync` notice on a client page, in the words stui and `st now` use (SyncPeer::summary
// in crates/st3-client/src/generated.rs): this host lacks records a peer holds, so what a page shows
// can be out of date, or it holds the same records as a peer and projects a different graph.

/** `1 envelope` or `124,384 envelopes`. */
export function envelopeCount(count: number): string {
  return `${count.toLocaleString('en-US')} envelope${count === 1 ? '' : 's'}`;
}

/** `caught up in about 14m`, rounded up so a short remainder never reads as done. */
export function catchUpEstimate(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined) return 'estimating time to catch up';
  const minutes = Math.ceil(seconds / 60);
  if (seconds === 0) return 'caught up';
  if (seconds < 60) return 'caught up in under a minute';
  if (minutes < 60) return `caught up in about ${minutes}m`;
  if (minutes < 24 * 60) return `caught up in about ${Math.floor(minutes / 60)}h ${minutes % 60}m`;
  return `caught up in about ${Math.floor(minutes / (24 * 60))}d ${Math.floor(minutes % (24 * 60) / 60)}h`;
}

const hostName = (peer: SyncPeer) => peer.host_id.replace(/^host\//, '');

/** `harbor has 26,820 envelopes this host lacks · caught up in about 14m`, or the diverged line. */
export function peerSummary(peer: SyncPeer): string {
  if (peer.diverged_since) return `${hostName(peer)} projects a different graph from the same envelopes`;
  return `${hostName(peer)} has ${envelopeCount(peer.peer_only_envelopes)} this host lacks · ${catchUpEstimate(peer.estimated_catch_up_seconds)}`;
}

export type SyncView = {
  /** Peers that hold records this host lacks, in the notice's words. A diverged peer is not here. */
  behind: string[];
  /** Peers that hold the same records and project a different graph; more exchanges cannot fix it. */
  diverged: string[];
};

/** What a notice says, or nothing for no notice. A peer with nothing the host lacks is not listed. */
export function syncView(notice: SyncNotice | null | undefined): SyncView {
  const view: SyncView = { behind: [], diverged: [] };
  for (const peer of notice?.peers ?? []) {
    if (peer.diverged_since) view.diverged.push(peerSummary(peer));
    else if (peer.peer_only_envelopes > 0) view.behind.push(peerSummary(peer));
  }
  return view;
}

/** The one line an app shows above its lists, or undefined when the host is level with its peers. */
export function syncLine(view: SyncView): string | undefined {
  if (view.diverged.length) return `This machine's view may be wrong: ${view.diverged.join('; ')}.`;
  if (view.behind.length) return `Catching up: ${view.behind.join('; ')}. What shows here can be out of date until then.`;
  return undefined;
}
