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
