// The tabs everyone sees, named and ordered as stui names them (crates/stui/src/ui/screens.rs).
export const TABS = ['Home', 'Agents', 'Missions', 'Fleet'] as const;
export type Tab = typeof TABS[number];

// Names earlier builds used, still accepted in links and stored preferences.
const ALIASES: Record<string, Tab> = { now: 'Home', chat: 'Agents', control: 'Missions' };

/** The tab a name means, in any case and under any earlier name, or null. */
export function tabNamed(name: string | null | undefined): Tab | null {
  if (!name) return null;
  const lower = name.trim().toLowerCase();
  return TABS.find(tab => tab.toLowerCase() === lower) ?? ALIASES[lower] ?? null;
}

/** A stored tab order: every tab once, in the stored order where it names them; else the default. */
export function tabOrder(stored: unknown): Tab[] {
  if (!Array.isArray(stored)) return [...TABS];
  const named = stored.map(value => typeof value === 'string' ? tabNamed(value) : null).filter((tab): tab is Tab => !!tab);
  const unique = [...new Set(named)];
  return unique.length === TABS.length ? unique : [...unique, ...TABS.filter(tab => !unique.includes(tab))];
}

export type DevLink =
  | { kind: 'tab'; tab: Tab }
  | { kind: 'scroll'; y: number }
  | { kind: 'mission'; id: string }
  | { kind: 'session'; id: string; terminal?: string }
  | { kind: 'agent'; id: string }
  | { kind: 'tree'; on: boolean }
  | { kind: 'pair'; gateway: string; id: string; code: string };

/** A Debug-only deep link, e.g. `com.compoundingtech.smalltalk.starter://tab/Agents`. */
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
    case 'tree': return { kind: 'tree', on: params.get('on') !== '0' };
    case 'pair': {
      const gateway = params.get('gateway'), id = params.get('id'), code = params.get('code');
      return gateway && id && code ? { kind: 'pair', gateway, id, code } : null;
    }
    default: return null;
  }
}
