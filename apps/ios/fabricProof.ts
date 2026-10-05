// A separate Debug smoke entry. It never accepts or persists a loopback gateway URL.
export type FabricPathMode = 'auto' | 'direct' | 'relay';
export type FabricProofInput = { node: string; service: string; address?: string; id?: string; code?: string; fullClient?: boolean; mode?: FabricPathMode };
export type FabricProfile = {
  url: string; credential: string; ready: boolean; close(): void;
  record(event: string, fields?: Record<string, string | number | boolean>): void;
};

export function parseFabricProofLink(link: string): FabricProofInput | null {
  let url: URL;
  try { url = new URL(link); } catch { return null; }
  if (!['com.compoundingtech.smalltalk.starter:', 'com.compoundingtech.smalltalk.fabricproof:'].includes(url.protocol) || url.hostname !== 'fabric-proof') return null;
  const node = url.searchParams.get('node'), service = url.searchParams.get('service');
  if (!node || !/^[a-f0-9]{64}$/i.test(node) || !service || new TextEncoder().encode(service).length > 255 || service.includes('\n') || service.includes('\0') || service.startsWith('git/')) return null;
  const mode = url.searchParams.get('mode');
  if (mode && !['auto', 'direct', 'relay'].includes(mode)) return null;
  const fullClient = url.searchParams.get('client');
  if (fullClient && fullClient !== '1') return null;
  const address = url.searchParams.get('addr') ?? undefined;
  const id = url.searchParams.get('id') ?? undefined, code = url.searchParams.get('code') ?? undefined;
  if (!!id !== !!code) return null;
  return { node, service, ...(mode ? { mode: mode as FabricPathMode } : {}), ...(fullClient ? { fullClient: true } : {}), ...(address ? { address } : {}), ...(id ? { id, code } : {}) };
}
