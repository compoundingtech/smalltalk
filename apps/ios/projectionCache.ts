import type { Agent, Attention, Device, Launch, Message, Mission, Runtime, Work } from '../../clients/typescript/st3-client';
import type { SessionView } from './sessionView';

export type MachineView = { id: string; kind: 'machine'; name: string; state: string; occupancy: { running_runtimes: number }; capacity: { state: string }; transports: Array<{ protocol: string; status: string }> };
export type Data = { attention: Attention[]; messages: Message[]; agents: Agent[]; missions: Mission[]; launches: Launch[]; machines: MachineView[]; devices: Device[]; sessions: SessionView[]; runtimes: Runtime[]; work: Work[] };
export const emptyData: Data = { attention: [], messages: [], agents: [], missions: [], launches: [], machines: [], devices: [], sessions: [], runtimes: [], work: [] };
export const PROJECTION_CACHE_KEY = 'st3.projection.v1';

const VERSION = 2;
const MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000;
const MAX_BYTES = 512 * 1024;
const LIMITS: Record<keyof Data, number> = { attention: 50, messages: 30, agents: 100, missions: 40, launches: 30, machines: 30, devices: 30, sessions: 100, runtimes: 80, work: 100 };
const kinds: Record<keyof Data, string> = { attention: 'attention', messages: 'message', agents: 'agent', missions: 'mission', launches: 'launch', machines: 'machine', devices: 'device', sessions: 'session', runtimes: 'runtime', work: 'work' };
const sensitiveKey = /credential|authorization|token|secret|private.?key|stream.?capability|stream.?url/i;

type Cache = { version: number; gateway: string; savedAt: number; hostId: string; actor: string; storeIndex: number; truncated: Array<keyof Data>; data: Data };

function stripSensitive(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(stripSensitive);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).filter(([key]) => !sensitiveKey.test(key)).map(([key, item]) => [key, stripSensitive(item)]));
  return value;
}

export function encodeProjectionCache(gateway: string, actor: string, hostId: string, storeIndex: number, data: Data, now = Date.now(), truncated: Array<keyof Data> = []): string | null {
  if (!/^https?:\/\//.test(gateway) || !actor || !Number.isFinite(storeIndex)) return null;
  const bounded = Object.fromEntries((Object.keys(kinds) as Array<keyof Data>).map(key => [key, data[key].slice(0, LIMITS[key])])) as Data;
  const clipped = (Object.keys(kinds) as Array<keyof Data>).filter(key => data[key].length > LIMITS[key]);
  const cache: Cache = { version: VERSION, gateway, savedAt: now, hostId, actor, storeIndex, truncated: [...new Set([...truncated, ...clipped])].filter(key => key in kinds), data: stripSensitive(bounded) as Data };
  const encoded = JSON.stringify(cache);
  return encoded.length <= MAX_BYTES ? encoded : null;
}

export function decodeProjectionCache(encoded: string | null, gateway: string, now = Date.now()): Cache | null {
  if (!encoded || encoded.length > MAX_BYTES) return null;
  let value: unknown;
  try { value = JSON.parse(encoded); } catch { return null; }
  if (!value || typeof value !== 'object') return null;
  const cache = value as Partial<Cache>;
  if (cache.version !== VERSION || cache.gateway !== gateway || typeof cache.actor !== 'string' || !cache.actor || typeof cache.hostId !== 'string' || !Number.isFinite(cache.storeIndex) || typeof cache.savedAt !== 'number' || cache.savedAt > now + 5 * 60 * 1000 || now - cache.savedAt > MAX_AGE_MS) return null;
  if (!cache.data || typeof cache.data !== 'object' || !Array.isArray(cache.truncated) || cache.truncated.length > Object.keys(kinds).length || cache.truncated.some(key => typeof key !== 'string' || !(key in kinds))) return null;
  for (const key of Object.keys(kinds) as Array<keyof Data>) {
    const entries = cache.data[key];
    if (!Array.isArray(entries) || entries.length > LIMITS[key] || entries.some(item => !item || typeof item !== 'object' || item.kind !== kinds[key] || typeof item.id !== 'string')) return null;
  }
  return cache as Cache;
}

export function hydrateProjectionForPairedDevice(encoded: string | null, gateway: string | null, hasCredential: boolean, now = Date.now()): Cache | null {
  return hasCredential && gateway ? decodeProjectionCache(encoded, gateway, now) : null;
}

export function offlinePresentation(hasCachedData: boolean): { title: string; detail: string } {
  return hasCachedData
    ? { title: 'Offline · showing last data', detail: 'Reconnecting in the background. Changes and actions need a live connection.' }
    : { title: 'Offline', detail: 'No data is cached yet. Reconnect to load your workspace.' };
}
