export type VisibleTab = 'Now' | 'Chat' | 'Control' | 'Fleet';
type EventSignal = { resource_ids: string[]; body?: unknown };
const tabs: VisibleTab[] = ['Now', 'Chat', 'Control', 'Fleet'];

export function tabsChangedByProjectionEvents(events: EventSignal[]): VisibleTab[] {
  return tabs.filter(tab => projectionEventsRequireRefresh(events, tab));
}

// Conversation changes have their own visible-session subscription. They must not
// force a full fleet projection refresh on every transcript event.
export function projectionEventsRequireRefresh(events: EventSignal[], tab?: VisibleTab): boolean {
  return events.some(event => {
    const body = event.body && typeof event.body === 'object' ? event.body as Record<string, unknown> : {};
    if (body.change === 'work.renewed') return false;
    if (!event.resource_ids.length) return true;
    const ids = event.resource_ids.filter(id => !id.startsWith('session/'));
    if (!ids.length) return false;
    if (!tab) return true;
    if (ids.some(id => id.startsWith('attention/'))) return true;
    if (tab === 'Now') return ids.some(id => id.startsWith('attention/') || id.startsWith('message/'));
    if (tab === 'Chat') return ids.some(id => id.startsWith('message/') || id.startsWith('step-run/')) || ids.some(id => id.startsWith('agent/'))
      && !(body.change === 'harness.observed' && ['ready', 'idle', 'working'].includes(String(body.state)));
    if (tab === 'Control') return ids.some(id => id.startsWith('mission/') || id.startsWith('step-run/') || id.startsWith('launch/'));
    return ids.some(id => id.startsWith('agent/') || id.startsWith('step-run/') || id.startsWith('runtime/') || id.startsWith('machine/') || id.startsWith('device/') || id.startsWith('terminal/'));
  });
}
