// A separate Debug smoke entry. It never accepts or persists a loopback gateway URL.
export type FabricProofInput = { node: string; service: string; address?: string; id?: string; code?: string };

export function parseFabricProofLink(link: string): FabricProofInput | null {
  let url: URL;
  try { url = new URL(link); } catch { return null; }
  if (url.protocol !== 'com.compoundingtech.smalltalk.starter:' || url.hostname !== 'fabric-proof') return null;
  const node = url.searchParams.get('node'), service = url.searchParams.get('service');
  if (!node || !/^[a-f0-9]{64}$/i.test(node) || !service || new TextEncoder().encode(service).length > 255 || service.includes('\n') || service.includes('\0') || service.startsWith('git/')) return null;
  const address = url.searchParams.get('addr') ?? undefined;
  const id = url.searchParams.get('id') ?? undefined, code = url.searchParams.get('code') ?? undefined;
  if (!!id !== !!code) return null;
  return { node, service, ...(address ? { address } : {}), ...(id ? { id, code } : {}) };
}
