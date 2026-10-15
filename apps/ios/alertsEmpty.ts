import { syncView } from '@smalltalk/st3-views';
import type { SyncRead } from './syncWatch';

/**
 * What Now says when it has no alerts. "All caught up" is a claim about the whole fleet, so it is
 * made only while connected, with the alerts loaded, and with the sync notice read and saying this
 * host is level with its peers. Until the notice is read, or while a peer holds records this host
 * lacks, the list may be missing alerts, and the screen says that instead.
 */
export function alertsEmptyText(state: { loadError?: string; status: string; hasSynced: boolean; sync: SyncRead }): string {
  if (state.loadError) return `Alerts could not be loaded: ${state.loadError}`;
  if (state.status === 'offline') return state.hasSynced ? 'No alerts in the offline snapshot.' : 'Connect to check for alerts.';
  if (!state.hasSynced) return 'Checking for alerts…';
  if (state.sync === undefined) return 'No alerts here yet. Checking whether this phone is up to date…';
  const view = syncView(state.sync);
  if (view.behind.length || view.diverged.length) return 'No alerts known. This view is out of date.';
  return 'No alerts. You’re all caught up.';
}
