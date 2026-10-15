// Where a member's client gateway is reached over fabric: the member's node ID, the fabric service
// its paired-only client socket is exposed as, and an optional address hint. It is saved beside the
// gateway the person paired. It is never a loopback URL: the only loopback address the app accepts
// is the one its own running native bridge returns.
export type FabricTarget = { node: string; service: string; address?: string };

const MAX_ADDRESS_BYTES = 2048;

function validService(service: string): boolean {
  // Fabric names a service by its protocol string. `git/` names are fabric's own repository
  // services, never a client gateway.
  return !!service && new TextEncoder().encode(service).length <= 255 && !/[\n\0]/.test(service) && !service.startsWith('git/');
}

/** The hint is the JSON text of an endpoint address the native module hands to fabric. */
function validAddress(address: string): boolean {
  if (new TextEncoder().encode(address).length > MAX_ADDRESS_BYTES) return false;
  try { const parsed = JSON.parse(address); return typeof parsed === 'object' && parsed !== null; } catch { return false; }
}

/** A target from saved or linked fields, or null when anything about it is not valid. */
export function fabricTarget(fields: { node?: unknown; service?: unknown; address?: unknown }): FabricTarget | null {
  const { node, service, address } = fields;
  if (typeof node !== 'string' || !/^[a-f0-9]{64}$/i.test(node)) return null;
  if (typeof service !== 'string' || !validService(service)) return null;
  if (address !== undefined && address !== null && address !== '') {
    if (typeof address !== 'string' || !validAddress(address)) return null;
    return { node: node.toLowerCase(), service, address };
  }
  return { node: node.toLowerCase(), service };
}

/** A target from a query string such as `node=…&service=…&addr=…`, as a pairing link or a build carries it. */
export function fabricTargetFromQuery(query: string | null | undefined): FabricTarget | null {
  if (!query) return null;
  const params = new URLSearchParams(query.replace(/^\?/, ''));
  return fabricTarget({ node: params.get('node'), service: params.get('service'), address: params.get('addr') ?? undefined });
}

/** A target typed or pasted by a person: a query string, or any link that carries one (`…?node=…&service=…`). */
export function fabricTargetFromText(text: string): FabricTarget | null {
  const trimmed = text.trim();
  const at = trimmed.indexOf('?');
  return fabricTargetFromQuery(at >= 0 ? trimmed.slice(at + 1) : trimmed);
}

/** What a build may carry: `EXPO_PUBLIC_ST3_FABRIC_DEFAULT`, set in an ignored env file on the build host. A repository build has none. */
export function buildFabricDefault(raw: string | undefined = process.env.EXPO_PUBLIC_ST3_FABRIC_DEFAULT): FabricTarget | null {
  return fabricTargetFromQuery(raw);
}

/** A target as it is stored. */
export function encodeFabricTarget(target: FabricTarget): string {
  return JSON.stringify(target.address ? { node: target.node, service: target.service, address: target.address } : { node: target.node, service: target.service });
}

/** A stored target, or null when it is missing or no longer valid. */
export function decodeFabricTarget(stored: string | null | undefined): FabricTarget | null {
  if (!stored) return null;
  try { return fabricTarget(JSON.parse(stored)); } catch { return null; }
}

/** The same member and service. An address hint does not make a different target. */
export function sameFabricTarget(a: FabricTarget | null | undefined, b: FabricTarget | null | undefined): boolean {
  return !!a && !!b && a.node === b.node && a.service === b.service;
}

/**
 * The identity of a device paired over fabric alone: it names the member and service, so the saved
 * gateway and the projection cache have something stable to key on. It is not a route and is never dialed.
 */
export function fabricGatewayId(target: FabricTarget): string {
  return `fabric://${target.node}/${encodeURIComponent(target.service)}`;
}
export function isFabricGatewayId(url: string): boolean { return url.startsWith('fabric://'); }

/** A short label for a screen: the service and the first bytes of the node ID. */
export function fabricTargetLabel(target: FabricTarget): string {
  return `${target.service} on ${target.node.slice(0, 8)}…`;
}

/**
 * A member the phone can reach over fabric: a target and the name the person saved it under. The
 * name is what the screen and a refusal say; a node ID or an address never is.
 *
 * Each credential goes only to the member it was paired with. `device` marks the one member that
 * uses this device's own pairing (the profile an older app build also uses with its single target);
 * every other member is paired on its own and keeps its own profile (`memberProfileKey`).
 */
export type FabricMember = FabricTarget & { name: string; device?: true };

/** The most members a phone keeps in its list. */
export const MAX_FABRIC_MEMBERS = 8;
const MAX_NAME_CHARS = 40;

/** A name as it is saved: trimmed, no control characters, at most 40 characters; empty when there is none. */
export function memberName(name: unknown): string {
  if (typeof name !== 'string') return '';
  return Array.from(name.replace(/[\u0000-\u001f\u007f]/g, ' ').trim()).slice(0, MAX_NAME_CHARS).join('').trim();
}

/** The name a member gets when it has none: its place in the list, as a person counts. */
export function defaultMemberName(index: number): string { return `member ${index + 1}`; }

function member(fields: unknown, index: number): FabricMember | null {
  if (!fields || typeof fields !== 'object') return null;
  const target = fabricTarget(fields as Record<string, unknown>);
  if (!target) return null;
  const record = fields as Record<string, unknown>;
  return { ...target, name: memberName(record.name) || defaultMemberName(index), ...(record.device === true ? { device: true as const } : {}) };
}

/** Whether two names are the same name to a person. */
export function sameName(a: string, b: string): boolean { return a.toLocaleLowerCase() === b.toLocaleLowerCase(); }

function uniqueName(kept: FabricMember[], index: number): string {
  for (let n = index; ; n++) { const name = defaultMemberName(n); if (!kept.some(existing => sameName(existing.name, name))) return name; }
}

/**
 * The ordered list, made safe: invalid entries and a second entry for the same member and service
 * are dropped, and the list is cut to its limit. The person's order is kept.
 */
export function fabricMembers(list: unknown): FabricMember[] {
  if (!Array.isArray(list)) return [];
  const kept: FabricMember[] = [];
  for (const fields of list) {
    const next = member(fields, kept.length);
    if (!next || kept.some(existing => sameFabricTarget(existing, next))) continue;
    // Names tell members apart in a refusal, so a repeated one gives way to the member's place.
    if (kept.some(existing => sameName(existing.name, next.name))) next.name = uniqueName(kept, kept.length);
    // Only one member uses this device's own pairing.
    if (next.device && kept.some(existing => existing.device)) delete next.device;
    kept.push(next);
    if (kept.length === MAX_FABRIC_MEMBERS) break;
  }
  return kept;
}

/**
 * The two stored values for a list: the list itself under the new key, and the member that uses this
 * device's own pairing under the old single-target key, exactly as an older app build wrote it. An
 * older build sends this device's credential to that target, so the old key never holds a member
 * paired on its own; with no such member it is removed, and an older build uses its saved gateway.
 */
export function storedFabricMembers(members: FabricMember[]): { members: string | null; target: string | null } {
  const list = fabricMembers(members);
  if (!list.length) return { members: null, target: null };
  const device = list.find(member => member.device);
  return {
    members: JSON.stringify(list.map(({ name, node, service, address, device }) => ({ name, node, service, ...(address ? { address } : {}), ...(device ? { device } : {}) }))),
    target: device ? encodeFabricTarget(device) : null,
  };
}

/**
 * The list from what is stored. The old key is the device's member as an older app build last saw
 * it: a single target an older build saved reads as a list of one that uses the device's pairing;
 * when an older build replaced or forgot that target after this app saved its list, the device's
 * member is replaced or forgotten too.
 */
export function readFabricMembers(storedList: string | null | undefined, storedTarget: string | null | undefined): FabricMember[] {
  let list: FabricMember[] = [];
  if (storedList) { try { list = fabricMembers(JSON.parse(storedList)); } catch { list = []; } }
  const old = decodeFabricTarget(storedTarget);
  if (!list.length) return old ? [{ ...old, name: defaultMemberName(0), device: true }] : [];
  const at = list.findIndex(member => member.device);
  if (!old) return at < 0 ? list : fabricMembers(list.filter((_, i) => i !== at));
  if (at >= 0 && sameFabricTarget(old, list[at])) return fabricMembers(list.map((member, i) => (i === at ? { ...old, name: member.name, device: true as const } : member)));
  // An older build chose another target: it is now the device's member, in the old one's place, or first.
  const plain = list.map(({ device: _, ...rest }) => rest as FabricMember);
  const known = plain.findIndex(member => sameFabricTarget(member, old));
  if (known >= 0) return fabricMembers(plain.map((member, i) => (i === known ? { ...old, name: member.name, device: true as const } : member)));
  // A different machine does not inherit the old one's name.
  const replaced: FabricMember = { ...old, name: defaultMemberName(at >= 0 ? at : 0), device: true };
  return fabricMembers(at >= 0 ? plain.map((member, i) => (i === at ? replaced : member)) : [replaced, ...plain]);
}

/** Where a member's own pairing is kept in the Keychain: one entry per member and service. */
export function memberProfileKey(target: FabricTarget): string {
  const service = Array.from(new TextEncoder().encode(target.service), byte => byte.toString(16).padStart(2, '0')).join('');
  return `st3.fabric.profile.${target.node}.${service}`;
}

/** How the phone is paired with a member: through this device's own pairing, its own pairing, or not yet. */
export type MemberPairing = 'device' | 'own' | 'none';

export function memberPairing(member: FabricMember, devicePaired: boolean, ownProfiles: ReadonlySet<string>): MemberPairing {
  if (member.device) return devicePaired ? 'device' : 'none';
  return ownProfiles.has(memberProfileKey(member)) ? 'own' : 'none';
}

/** The members the walk may dial, in the person's order: only those this phone is paired with. */
export function walkableMembers(members: FabricMember[], devicePaired: boolean, ownProfiles: ReadonlySet<string>): FabricMember[] {
  return members.filter(member => memberPairing(member, devicePaired, ownProfiles) !== 'none');
}

/**
 * The credential the client may send. Over the saved gateway, this device's own. Over fabric, this
 * device's own only to the member that uses it, a member's own only to that member, and none to a
 * member not paired yet or one that cannot be named.
 */
export function credentialFor(overFabric: boolean, member: FabricMember | null, ownCredential: string | null | undefined, deviceCredential: string | null): string | null {
  if (!overFabric) return deviceCredential;
  if (!member) return null;
  return member.device ? deviceCredential : ownCredential ?? null;
}

/**
 * The list a build opens with when nothing is saved: its default target as the member that uses this
 * device's own pairing. A phone paired through its form has only that pairing, so the built target
 * must use it; as a member of its own it would wait for a pairing it can never have, and the carrier
 * would never dial it.
 */
export function builtFabricMembers(built: FabricTarget | null): FabricMember[] {
  return built ? [{ ...built, name: defaultMemberName(0), device: true }] : [];
}
