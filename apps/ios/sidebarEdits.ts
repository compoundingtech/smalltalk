import { ClientError, SIDEBAR_ARRANGEMENT_UUID, type Arrangement, type ArrangementOperation } from '../../clients/typescript/st3-client';

/** What a person reads when their Sidebar was removed and no older one exists to edit. */
export const SIDEBAR_RETIRED = 'This Sidebar was removed; restoring a removed Sidebar is not available yet.';
/** Whether st refused to create the Sidebar because the person retired it. Retired arrangements
 *  leave the window, so this refusal is how the phone learns it. */
export function sidebarRetired(error: unknown): boolean { return error instanceof ClientError && error.response.code === 'arrangement-retired'; }

/** The subject of a person's one Sidebar (#2043); concurrent creates converge on it. */
export function sidebarSubject(person: string): string { return `arrangement/${person}/${SIDEBAR_ARRANGEMENT_UUID}`; }

/** The reserved subject whatever its name, else the oldest live arrangement named "Sidebar",
 *  which clients created before the reserved id. Every client picks the same one. */
export function sidebarArrangement(arrangements: Arrangement[]): Arrangement | undefined {
  const live = arrangements.filter(a => !a.deleted);
  return live.find(a => a.id.endsWith(`/${SIDEBAR_ARRANGEMENT_UUID}`))
    ?? live.filter(a => a.body.name.value === 'Sidebar').sort((a, b) => compare(a.id, b.id))[0];
}
const digits = '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz';
const integerLength = (key: string) => key[0] >= 'a' ? key.charCodeAt(0) - 95 : 92 - key.charCodeAt(0);
function stepInteger(key: string, direction: number): string | null {
  const head = key[0], tail = key.slice(1).split('');
  for (let i = tail.length - 1; i >= 0; i--) {
    const next = digits.indexOf(tail[i]) + direction;
    if (next >= 0 && next < 62) { tail[i] = digits[next]; return head === 'A' && tail.every(d => d === '0') ? null : head + tail.join(''); }
    tail[i] = direction > 0 ? '0' : 'z';
  }
  if (direction > 0 && head === 'Z') return 'a0';
  if (direction < 0 && head === 'a') return 'Zz';
  if (direction > 0 && head === 'z' || direction < 0 && head === 'A') return null;
  const nextHead = String.fromCharCode(head.charCodeAt(0) + direction);
  const length = integerLength(nextHead);
  return nextHead + (direction > 0 ? '0' : 'z').repeat(length - 1);
}
function midpoint(a: string, b: string | null): string {
  let prefix = '', index = 0;
  while (b !== null && (a[index] ?? '0') === b[index]) { prefix += b[index++]; }
  const low = a[index] ? digits.indexOf(a[index]) : 0;
  const high = b !== null && b[index] ? digits.indexOf(b[index]) : 62;
  if (high - low > 1) return prefix + digits[Math.floor((low + high) / 2)];
  if (b !== null && b.length > index + 1) return prefix + b[index];
  return prefix + digits[low] + midpoint(a.slice(index + 1), null);
}
/** Canonical base-62 fractional keys, with a bounded refusal rather than a malformed edit. */
export function keyBetween(a: string | null, b: string | null): string {
  if (a !== null && b !== null && a >= b) throw new Error('These rows have equal position keys; reorder their group first.');
  let key: string;
  if (a === null) {
    if (b === null) return 'a0';
    const integer = b.slice(0, integerLength(b));
    const previous = stepInteger(integer, -1);
    if (integer === b && previous === null) throw new Error('This ordering gap is full.');
    key = integer < b ? integer : previous!;
  } else {
    const integer = a.slice(0, integerLength(a)), fraction = a.slice(integer.length);
    if (b === null) key = stepInteger(integer, 1) ?? integer + midpoint(fraction, null);
    else {
      const upperInteger = b.slice(0, integerLength(b));
      const next = stepInteger(integer, 1);
      key = integer === upperInteger ? integer + midpoint(fraction, b.slice(integer.length)) : next && next < b ? next : integer + midpoint(fraction, null);
    }
  }
  if (a !== null && a >= key || b !== null && key >= b) throw new Error('This ordering gap is full.');
  if (key.length > 128) throw new Error('This ordering gap is full; reorder the group first.');
  return key;
}
function insertion(keys: string[], index: number): { key: string; rekeys: Array<[number, string]> } {
  try { return { key: keyBetween(keys[index - 1] ?? null, keys[index] ?? null), rekeys: [] }; }
  catch (error) {
    if (keys.length >= 1024) throw new Error('This collision needs more register edits than one action allows.');
    const positioned = Array.from({ length: keys.length + 1 }, (_, n) => 'c' + digits[Math.floor(n / (62 * 62))] + digits[Math.floor(n / 62) % 62] + digits[n % 62]);
    return { key: positioned[index], rekeys: keys.map((_, n) => [n, positioned[n < index ? n : n + 1]]) };
  }
}
export const folderLocation = (a: Arrangement | undefined, subject: string): string | null => {
  const resolved = a?.resolved?.folders;
  const folder = resolved && subject in resolved ? resolved[subject] : a?.body.placements[subject]?.value.folder;
  return folder && a?.body.folders[folder] && !a.body.folders[folder].tombstone ? folder : null;
};
export function folderName(name: string): string {
  if (!name.trim() || /[\u0000-\u001f\u007f-\u009f]/u.test(name) || [...name.trim()].reduce((n, char) => n + (char.codePointAt(0)! < 128 ? 1 : char.codePointAt(0)! < 2048 ? 2 : char.codePointAt(0)! < 65536 ? 3 : 4), 0) > 256) throw new Error('Use a folder name of at most 256 bytes without control characters.');
  return name.trim();
}
export function folderCreate(a: Arrangement | undefined, id: string, name: string): ArrangementOperation[] {
  name = folderName(name);
  const siblings = Object.values(a?.body.folders ?? {}).filter(f => !f.tombstone).map(f => f.position.value.key).sort(compare);
  return [{ op: 'folder.create', id, name: name.trim(), parent: null, key: keyBetween(siblings.at(-1) ?? null, null) }];
}
/** Place before an anchor or append, including dormant placement keys so they never collide. */
export function placeResource(a: Arrangement | undefined, subject: string, folder: string | null, before?: string, after?: string): ArrangementOperation[] {
  const siblings = Object.entries(a?.body.placements ?? {}).filter(([id]) => id !== subject && folderLocation(a, id) === folder).sort(([ai, av], [bi, bv]) => compare(av.value.key, bv.value.key) || compare(ai, bi));
  const anchors: ArrangementOperation[] = [];
  const anchor = before ?? after;
  if (anchor && !siblings.some(([id]) => id === anchor) && folder === null) {
    const key = keyBetween(siblings.at(-1)?.[1].value.key ?? null, null);
    siblings.push([anchor, { value: { folder: null, key }, revision: '' }]);
    anchors.push({ op: 'subject.place', subject: anchor, folder: null, key });
  }
  if (anchor && !siblings.some(([id]) => id === anchor)) throw new Error('The destination changed; choose it again.');
  const index = before ? siblings.findIndex(([id]) => id === before) : after ? siblings.findIndex(([id]) => id === after) + 1 : siblings.length;
  if (index < 0) throw new Error('The destination changed; choose it again.');
  const placed = insertion(siblings.map(([, value]) => value.value.key), index);
  // Each register appears at most once, including an unfiled anchor introduced in this edit.
  const operations = new Map<string, ArrangementOperation>();
  for (const anchor of anchors) operations.set((before ?? after)!, anchor);
  for (const [at, key] of placed.rekeys) operations.set(siblings[at][0], { op: 'subject.place', subject: siblings[at][0], folder, key });
  operations.set(subject, { op: 'subject.place', subject, folder, key: placed.key });
  return [...operations.values()];
}
export function moveFolder(a: Arrangement, id: string, before?: string): ArrangementOperation[] {
  const siblings = Object.entries(a.body.folders).filter(([key, f]) => key !== id && !f.tombstone).sort(([ai, av], [bi, bv]) => compare(av.position.value.key, bv.position.value.key) || compare(ai, bi));
  const index = before ? siblings.findIndex(([key]) => key === before) : siblings.length;
  if (index < 0) throw new Error('The destination changed; choose it again.');
  const inserted = insertion(siblings.map(([, value]) => value.position.value.key), index);
  return [...inserted.rekeys.map(([at, key]) => ({ op: 'folder.move' as const, id: siblings[at][0], parent: siblings[at][1].position.value.parent, key })), { op: 'folder.move', id, parent: a.body.folders[id].position.value.parent, key: inserted.key }];
}
function compare(a: string, b: string): number { return a < b ? -1 : a > b ? 1 : 0; }
/** UUIDv7 construction from this device's wall clock and cryptographic random bytes. */
export function uuid7(now: number, random: Uint8Array): string {
  const bytes = new Uint8Array(16); let time = now;
  for (let i = 5; i >= 0; i--) { bytes[i] = time % 256; time = Math.floor(time / 256); }
  bytes.set(random.slice(0, 10), 6); bytes[6] = (bytes[6] & 15) | 112; bytes[8] = (bytes[8] & 63) | 128;
  const hex = [...bytes].map(v => v.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
