import type { Arrangement } from '../../clients/typescript/st3-client';

export type SidebarKind = 'Agents' | 'Missions' | 'Terminals' | 'Machines' | 'Unavailable';
export type SidebarResource = { id: string; title: string; kind: SidebarKind; target: string; missing?: boolean };
export type SidebarGroup = { id: string; title: string; resources: SidebarResource[]; collapsed: boolean; folder: boolean };
export type SidebarChoices = Record<string, boolean>;
const kinds: SidebarKind[] = ['Agents', 'Missions', 'Terminals', 'Machines', 'Unavailable'];

/** A subject leaving a current window remains visible, marked unavailable. */
export function rememberResources(previous: Map<string, SidebarResource>, current: SidebarResource[]): Map<string, SidebarResource> {
  const next = new Map([...previous].map(([id, row]) => [id, { ...row, missing: true }]));
  for (const row of current) next.set(row.id, { ...row, missing: false });
  return next;
}

/** One deterministic Sidebar arrangement, flattened to one folder level without rewriting it. */
export function sidebarGroups(arrangements: Arrangement[], resources: Map<string, SidebarResource>, choices: SidebarChoices = {}, filter = ''): { groups: SidebarGroup[]; everythingCollapsed: boolean; unfiledCount: number } {
  const arrangement = [...arrangements].filter(a => !a.deleted).sort((a, b) => Number(a.body.name.value !== 'Sidebar') - Number(b.body.name.value !== 'Sidebar') || compare(a.id, b.id))[0];
  const catalog = new Map(resources);
  for (const id of Object.keys(arrangement?.body.placements ?? {})) if (!catalog.has(id)) catalog.set(id, { id, title: id, kind: 'Unavailable', target: id, missing: true });
  const folders = Object.entries(arrangement?.body.folders ?? {}).filter(([, folder]) => !folder.tombstone).sort(([aid, a], [bid, b]) => compare(a.position.value.key, b.position.value.key) || compare(aid, bid));
  const location = (id: string): string | null => {
    const placement = arrangement?.body.placements[id];
    if (!placement) return null;
    const folder = arrangement?.resolved?.folders[id] ?? placement.value.folder;
    // A resolved null is authoritative, even if a raw register still points to a deleted folder.
    const resolved = arrangement?.resolved?.folders;
    const effective = resolved && id in resolved ? resolved[id] : folder;
    return effective && folders.some(([id]) => id === effective) ? effective : null;
  };
  const query = filter.trim().toLowerCase();
  const matches = (row: SidebarResource) => !query || row.title.toLowerCase().includes(query) || row.id.toLowerCase().includes(query);
  const group = (id: string, title: string, rows: SidebarResource[], folder: boolean): SidebarGroup => ({ id, title, resources: rows.filter(matches), folder, collapsed: !query && (choices[id] ?? (!folder && rows.length > 30)) });
  const groups = folders.map(([id, folder]) => group(`folder/${id}`, folder.name.value, [...catalog.values()].filter(row => location(row.id) === id).sort((a, b) => compare(arrangement!.body.placements[a.id].value.key, arrangement!.body.placements[b.id].value.key) || compare(a.id, b.id)), true));
  const unfiled = [...catalog.values()].filter(row => !location(row.id));
  for (const kind of kinds) {
    const rows = unfiled.filter(row => row.kind === kind).sort((a, b) => compare(a.title, b.title) || compare(a.id, b.id));
    if (rows.length) groups.push(group(`kind/${kind}`, kind, rows, false));
  }
  return { groups, everythingCollapsed: !query && (choices.everything ?? unfiled.length > 30), unfiledCount: unfiled.length };
}
function compare(a: string, b: string): number { return a < b ? -1 : a > b ? 1 : 0; }
