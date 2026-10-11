// The phone reaches its member over one of two carriers: the saved Tailscale, LAN or HTTPS gateway
// (the default), or fabric, an opt-in. A fabric carrier opens a fresh native listener each time the
// app is in the foreground and stops it in the background. When fabric cannot be used the app
// falls back to the saved gateway, and always says which route is live and why.
import { gatewayTransport } from './gatewayUrl';
import type { FabricTarget } from './fabricTarget';

export type CarrierChoice = 'tailscale' | 'fabric';
export type FabricPath = 'direct' | 'relay' | 'unknown';
export type FabricPhase = 'off' | 'idle' | 'dialing' | 'ready' | 'failed' | 'refused';
export type FabricSnapshot = { phase: FabricPhase; bridgeUrl: string; path: FabricPath; reason: string };

export type Route =
  | { kind: 'none'; fellBack: false; why: '' }
  | { kind: 'tailnet' | 'lan' | 'https'; fellBack: boolean; why: string }
  | { kind: 'fabric'; path: FabricPath; fellBack: false; why: '' };

export type CarrierPorts = {
  /** Open the native bridge to the target; its loopback listener and, when known, the path in use. */
  dial(target: FabricTarget): Promise<{ url: string; path?: FabricPath }>;
  stop(): Promise<void>;
  /** Whether the member's admission refused this phone since the last dial. */
  refused(): Promise<boolean>;
  now(): number;
  wait(ms: number): Promise<void>;
};

/** How long the native dial may take before the app uses its saved route instead. */
export const DIAL_BUDGET_MS = 4000;
/** After fabric failed, how long the next foreground goes straight to the saved route. */
export const REMEMBER_FAILURE_MS = 30_000;
/** Waits before each of the redials after the feed lost a ready fabric connection. */
export const REDIAL_WAITS_MS = [1000, 2000, 5000];

const OFF: FabricSnapshot = { phase: 'off', bridgeUrl: '', path: 'unknown', reason: '' };

function reasonOf(error: unknown): string {
  return error instanceof Error && error.message ? error.message : 'Fabric could not connect';
}

export class FabricCarrier {
  private snapshotValue: FabricSnapshot = OFF;
  private target: FabricTarget | null = null;
  private enabled = false;
  private active = false;
  private epoch = 0;
  private redials = 0;
  private failedAt = -Infinity;
  private failedReason = '';
  private running = false;

  constructor(private readonly ports: CarrierPorts, private readonly changed: (snapshot: FabricSnapshot) => void) {}

  get snapshot(): FabricSnapshot { return this.snapshotValue; }

  private publish(next: Partial<FabricSnapshot>): void {
    this.snapshotValue = { ...this.snapshotValue, ...next };
    this.changed(this.snapshotValue);
  }

  /** The carrier choice, and the saved target, as the person set them. */
  configure(enabled: boolean, target: FabricTarget | null): void {
    const a = this.target, b = target;
    const same = a === b || (!!a && !!b && a.node === b.node && a.service === b.service && a.address === b.address);
    if (enabled === this.enabled && same) return;
    this.enabled = enabled;
    this.target = target;
    this.failedAt = -Infinity;
    this.redials = 0;
    void this.reconcile(true);
  }

  foreground(active: boolean): void {
    if (active === this.active) return;
    this.active = active;
    void this.reconcile(false);
  }

  /** The person asks for fabric again after a failure or refusal. */
  retry(): void { this.failedAt = -Infinity; this.redials = 0; void this.reconcile(true); }

  /** The feed over the bridge reports a good connection: redials start over. */
  feedLive(): void { this.redials = 0; }

  /** The feed over the bridge lost its connection. A refusal ends the trial; anything else is redialed. */
  async feedFailed(): Promise<void> {
    if (this.snapshotValue.phase !== 'ready') return;
    const epoch = this.epoch;
    let refused = false;
    try { refused = await this.ports.refused(); } catch { /* a stats failure is not a refusal */ }
    if (epoch !== this.epoch) return;
    if (refused) {
      this.epoch++;
      this.running = false;
      await this.ports.stop();
      this.publish({ phase: 'refused', bridgeUrl: '', reason: 'The member refused fabric access. Check that its grant for this phone is still there.' });
      return;
    }
    if (this.redials >= REDIAL_WAITS_MS.length) {
      this.epoch++;
      this.running = false;
      await this.ports.stop();
      this.failedAt = this.ports.now();
      this.failedReason = 'Fabric lost its connection to the member';
      this.publish({ phase: 'failed', bridgeUrl: '', reason: this.failedReason });
      return;
    }
    const wait = REDIAL_WAITS_MS[this.redials++];
    this.epoch++;
    this.running = false;
    await this.ports.stop();
    this.publish({ phase: 'dialing', bridgeUrl: '', reason: '' });
    const mine = this.epoch;
    await this.ports.wait(wait);
    if (mine !== this.epoch) return;
    void this.reconcile(true);
  }

  private async reconcile(restart: boolean): Promise<void> {
    const wanted = this.enabled && this.active && !!this.target;
    if (!wanted) {
      const wasRunning = this.running;
      this.epoch++;
      this.running = false;
      if (wasRunning) await this.ports.stop();
      this.publish({ phase: this.enabled && this.target ? 'idle' : 'off', bridgeUrl: '', reason: '' });
      return;
    }
    if (this.running && !restart) return;
    await this.start();
  }

  private async start(): Promise<void> {
    const target = this.target;
    if (!target) return;
    const epoch = ++this.epoch;
    if (this.running) await this.ports.stop();
    if (epoch !== this.epoch) return;
    this.running = false;
    // A fabric that just failed is not waited on again at every foreground.
    if (this.ports.now() - this.failedAt < REMEMBER_FAILURE_MS) {
      this.publish({ phase: 'failed', bridgeUrl: '', reason: this.failedReason });
      return;
    }
    this.publish({ phase: 'dialing', bridgeUrl: '', reason: '' });
    const budget = this.ports.wait(DIAL_BUDGET_MS).then(() => 'late' as const);
    try {
      this.running = true;
      const outcome = await Promise.race([this.ports.dial(target), budget]);
      if (epoch !== this.epoch) { if (outcome !== 'late') await this.ports.stop(); return; }
      if (outcome === 'late') {
        this.running = false;
        await this.ports.stop();
        this.failedAt = this.ports.now();
        this.failedReason = `Fabric did not answer within ${Math.round(DIAL_BUDGET_MS / 1000)} s`;
        this.publish({ phase: 'failed', bridgeUrl: '', reason: this.failedReason });
        return;
      }
      this.publish({ phase: 'ready', bridgeUrl: outcome.url, path: outcome.path ?? 'unknown', reason: '' });
    } catch (error) {
      if (epoch !== this.epoch) return;
      this.running = false;
      this.failedAt = this.ports.now();
      this.failedReason = reasonOf(error);
      await this.ports.stop().catch(() => {});
      this.publish({ phase: 'failed', bridgeUrl: '', reason: this.failedReason });
    }
  }
}

export type Selected = { baseUrl: string | null; route: Route; /** Fabric is being opened; nothing is dialed over the saved route meanwhile. */ pending: boolean; issue: string };

function savedRoute(saved: string, fellBack: boolean, why: string): Route {
  const kind = gatewayTransport(saved);
  return { kind: kind === 'tailnet' || kind === 'lan' ? kind : 'https', fellBack, why };
}

/**
 * The URL the client uses and the route that names it. With fabric chosen and ready, the bridge;
 * while fabric opens, nothing yet; when fabric cannot be used and a gateway is saved, that gateway,
 * named as a fallback with the reason (so a refusal or an outage is always visible).
 */
export function selectRoute(choice: CarrierChoice, fallback: boolean, saved: string | null, fabric: FabricSnapshot, hasTarget: boolean): Selected {
  if (choice === 'tailscale') {
    return saved ? { baseUrl: saved, route: savedRoute(saved, false, ''), pending: false, issue: '' } : { baseUrl: null, route: { kind: 'none', fellBack: false, why: '' }, pending: false, issue: '' };
  }
  if (!hasTarget) {
    const why = 'No fabric target is saved, so the saved gateway is used';
    return saved ? { baseUrl: saved, route: savedRoute(saved, true, why), pending: false, issue: '' } : { baseUrl: null, route: { kind: 'none', fellBack: false, why: '' }, pending: false, issue: why };
  }
  switch (fabric.phase) {
    case 'ready':
      return { baseUrl: fabric.bridgeUrl, route: { kind: 'fabric', path: fabric.path, fellBack: false, why: '' }, pending: false, issue: '' };
    case 'off':
    case 'idle':
    case 'dialing':
      return { baseUrl: null, route: { kind: 'none', fellBack: false, why: '' }, pending: true, issue: '' };
    case 'failed':
    case 'refused':
      if (fallback && saved) return { baseUrl: saved, route: savedRoute(saved, true, fabric.reason), pending: false, issue: '' };
      return { baseUrl: null, route: { kind: 'none', fellBack: false, why: '' }, pending: false, issue: fabric.reason };
  }
}

/** The route in words, for the connection screen. */
export function routeLabel(route: Route): string {
  switch (route.kind) {
    case 'fabric': return route.path === 'unknown' ? 'Fabric' : `Fabric (${route.path})`;
    case 'tailnet': return route.fellBack ? 'Tailscale, because fabric could not be used' : 'Tailscale';
    case 'lan': return route.fellBack ? 'Local network, because fabric could not be used' : 'Local network';
    case 'https': return route.fellBack ? 'HTTPS gateway, because fabric could not be used' : 'HTTPS gateway';
    case 'none': return 'Not connected';
  }
}
