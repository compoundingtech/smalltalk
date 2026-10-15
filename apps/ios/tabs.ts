// The four phone tabs. Earlier tab names and preferences are intentionally retired.
export const TABS = ['Now', 'Resources', 'Fleet', 'Spaces'] as const;
export type Tab = typeof TABS[number];
export const tabLabel = (tab: Tab): string => tab;
export function tabNamed(name: string | null | undefined): Tab | null {
  return TABS.find(tab => tab.toLowerCase() === name?.trim().toLowerCase()) ?? null;
}
export function tabOrder(stored: unknown): Tab[] {
  if (!Array.isArray(stored) || stored.length !== TABS.length) return [...TABS];
  const named = stored.map(value => typeof value === 'string' ? tabNamed(value) : null);
  return named.every((tab): tab is Tab => !!tab) && new Set(named).size === TABS.length ? named : [...TABS];
}

export type DevLink =
  | { kind: 'tab'; tab: Tab }
  | { kind: 'scroll'; y: number }
  | { kind: 'mission'; id: string }
  | { kind: 'session'; id: string; terminal?: string }
  | { kind: 'agent'; id: string }
  | { kind: 'terminal'; id: string }
  | { kind: 'attention'; id: string }
  | { kind: 'pair'; gateway: string; id: string; code: string };

/** A Debug-only deep link, e.g. `com.compoundingtech.smalltalk.starter://tab/Resources`. */
export function parseDevLink(link: string): DevLink | null {
  let parsed: URL;
  try { parsed = new URL(link); } catch { return null; }
  const params = parsed.searchParams;
  const path = parsed.pathname.replace(/^\/+/, '');
  switch (parsed.hostname) {
    case 'tab': { const tab = tabNamed(path); return tab ? { kind: 'tab', tab } : null; }
    case 'scroll': { const y = Number(params.get('y')); return Number.isFinite(y) && y >= 0 ? { kind: 'scroll', y } : null; }
    case 'mission': { const id = params.get('id'); return id?.startsWith('mission/') ? { kind: 'mission', id } : null; }
    case 'session': {
      const id = params.get('id'), terminal = params.get('terminal');
      return id?.startsWith('session/') ? { kind: 'session', id, ...(terminal?.startsWith('terminal/') ? { terminal } : {}) } : null;
    }
    case 'agent': { const id = params.get('id'); return id?.startsWith('agent/') ? { kind: 'agent', id } : null; }
    case 'terminal': { const id = params.get('id'); return id?.startsWith('terminal/') ? { kind: 'terminal', id } : null; }
    case 'attention': { const id = params.get('id'); return id?.startsWith('attention/') ? { kind: 'attention', id } : null; }
    case 'pair': {
      const gateway = params.get('gateway'), id = params.get('id'), code = params.get('code');
      return gateway && id && code ? { kind: 'pair', gateway, id, code } : null;
    }
    default: return null;
  }
}
