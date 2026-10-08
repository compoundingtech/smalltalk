import { normalizeGatewayUrl } from './gatewayUrl.ts';
import type { ForegroundGate } from './foreground.ts';

export const DAILY_APP = 'com.compoundingtech.smalltalk';
export const DAILY_CHANNEL = 'daily';
export type AppUpdateToken = { token: string; expiresAtUnixMs: number };
export type AppUpdatePort = {
  enabled: boolean;
  mint(gateway: string, credential: string): Promise<AppUpdateToken>;
  setGateway(gateway: string): Promise<void>;
  setToken(token: AppUpdateToken | null): void;
  check(): Promise<{ isAvailable: boolean; isRollBackToEmbedded: boolean }>;
  fetch(): Promise<{ isNew: boolean; isRollBackToEmbedded: boolean }>;
  reload(): Promise<void>;
  consent(apply: () => void): void;
  onFailure(): void;
  now(): number;
};

/** This is the only request allowed to carry the broad Keychain-backed paired credential. */
export const mintAppUpdateToken = async (gateway: string, credential: string, fetchImpl: typeof fetch = fetch): Promise<AppUpdateToken> => {
  const response = await fetchImpl(`${gateway}/v1/client/app-updates/token`, {
    method: 'POST',
    redirect: 'error',
    headers: { Authorization: `Bearer ${credential}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ app: DAILY_APP, channel: DAILY_CHANNEL }),
  });
  if (!response.ok) throw new Error('App update authorization is unavailable');
  const envelope: unknown = await response.json();
  if (typeof envelope !== 'object' || envelope === null || !('value' in envelope)) {
    throw new Error('Invalid app update authorization response');
  }
  const body = envelope.value;
  if (typeof body !== 'object' || body === null || !('token' in body) || !('expiresAtUnixMs' in body)
    || typeof body.token !== 'string' || !body.token || /[\r\n]/.test(body.token)
    || typeof body.expiresAtUnixMs !== 'number' || !Number.isSafeInteger(body.expiresAtUnixMs)) {
    throw new Error('Invalid app update authorization response');
  }
  return { token: body.token, expiresAtUnixMs: body.expiresAtUnixMs };
};

/** One serialized native check/download, never an automatic reload of somebody's active work. */
export class AppUpdateSession {
  private pairing: { gateway: string; credential: string } | undefined;
  private generation = 0;
  private closed = false;
  private running: Promise<void> | undefined;
  private requested = false;
  private ready: { generation: number; prompted: boolean } | undefined;
  private unsubscribe: () => void;

  constructor(private readonly port: AppUpdatePort, private readonly foreground: ForegroundGate) {
    this.unsubscribe = foreground.subscribe(active => {
      if (!active) return;
      if (this.ready) this.offerReload();
      else void this.check();
    });
    if (port.enabled) port.setToken(null); // Clear in-memory authorization before a new pairing read.
  }

  /** Called only with the result of the store's SecureStore read or completed verified pairing. */
  setPairing(gateway: string, credential: string | null): void {
    const normalized = normalizeGatewayUrl(gateway);
    // The gateway API and publisher use fixed root routes, never a reverse-proxy path prefix.
    const next = normalized && new URL(normalized).pathname === '/' && credential ? { gateway: normalized, credential } : undefined;
    if (next?.gateway === this.pairing?.gateway && next?.credential === this.pairing?.credential) return;
    this.pairing = next;
    this.generation++;
    this.ready = undefined;
    if (this.port.enabled) this.port.setToken(null);
    if (next && this.running) this.requested = true;
    if (next) void this.check();
  }

  check(): Promise<void> {
    if (this.closed || !this.port.enabled || !this.pairing || !this.foreground.active || this.ready) return Promise.resolve();
    if (this.running) return this.running;
    this.requested = true;
    this.running = this.drain().finally(() => { this.running = undefined; });
    return this.running;
  }

  private async drain(): Promise<void> {
    while (this.requested && !this.closed) {
      this.requested = false;
      const pairing = this.pairing, generation = this.generation;
      const current = () => !this.closed && generation === this.generation;
      if (!pairing || !this.foreground.active) continue;
      try {
        const token = await this.port.mint(pairing.gateway, pairing.credential);
        if (!current() || !this.foreground.active) continue;
        const remaining = token.expiresAtUnixMs - this.port.now();
        if (remaining <= 0 || remaining > 15 * 60 * 1000) throw new Error('Invalid app update token expiry');
        await this.port.setGateway(pairing.gateway);
        if (!current() || !this.foreground.active) continue;
        // Only the transport sees this bearer; Expo's compared/persisted config stays build-owned.
        this.port.setToken(token);
        const available = await this.port.check();
        if (!current() || !this.foreground.active || token.expiresAtUnixMs <= this.port.now()) continue;
        if (!available.isAvailable && !available.isRollBackToEmbedded) continue;
        const downloaded = await this.port.fetch();
        if (!current() || (!downloaded.isNew && !downloaded.isRollBackToEmbedded)) continue;
        this.ready = { generation, prompted: false };
        this.offerReload();
      } catch {
        if (current()) this.port.onFailure(); // No credentials, URLs or response bodies in diagnostics.
      } finally {
        this.port.setToken(null);
      }
    }
  }

  private offerReload(): void {
    const ready = this.ready;
    if (!ready || ready.prompted || this.closed || !this.foreground.active) return;
    ready.prompted = true;
    this.port.consent(() => {
      if (this.closed || ready.generation !== this.generation || !this.foreground.active) return;
      void this.port.reload().catch(() => this.port.onFailure());
    });
  }

  close(): void {
    this.closed = true;
    this.generation++;
    this.pairing = undefined;
    this.ready = undefined;
    this.unsubscribe();
    if (this.port.enabled) this.port.setToken(null);
  }
}
