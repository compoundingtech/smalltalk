// The phone reaches its member over one of two carriers: the saved Tailscale, LAN or HTTPS gateway
// (the default), or fabric, an opt-in. A fabric carrier opens a fresh native listener each time the
// app is in the foreground and stops it in the background. When fabric cannot be used the app
// falls back to the saved gateway, and always says which route is live and why.
//
// Fabric reaches one of an ordered list of members. The carrier walks the list in the person's
// order until a member answers. A member that refuses the phone stops the walk: nothing after it is
// dialed, and the refusal names the member. Only the person moves on, by picking another member.
// The list the carrier walks holds only members this phone is paired with (see fabricTarget.ts).
import { gatewayTransport } from './gatewayUrl';
import { sameFabricTarget, type FabricMember, type FabricTarget } from './fabricTarget';

export type CarrierChoice = 'tailscale' | 'fabric';
export type FabricPath = 'direct' | 'relay' | 'unknown';
export type FabricPhase = 'off' | 'idle' | 'dialing' | 'ready' | 'failed' | 'refused';
export type FabricSnapshot = {
  phase: FabricPhase; bridgeUrl: string; path: FabricPath; reason: string;
  /** The saved name of the member being dialed, in use, or that refused; empty otherwise. */
  member: string;
  /** How many members the last walk dialed, a count only. */
  tried: number;
};

export type Route =
  | { kind: 'none'; fellBack: false; why: '' }
  | { kind: 'tailnet' | 'lan' | 'https'; fellBack: boolean; why: string }
  | { kind: 'fabric'; path: FabricPath; fellBack: false; why: '' };

/** A member's fabric admission refused this phone; `code` is fabric's typed code when it sent one. */
export class FabricRefusal extends Error {
  constructor(readonly code: string | null) { super('The member refused fabric access'); this.name = 'FabricRefusal'; }
}

export type CarrierPorts = {
  /**
   * Open the native bridge to the member and make sure it answers: resolve once the member admitted
   * the phone, with the loopback listener and, when known, the path in use; reject with a
   * FabricRefusal when the member refused the phone, and with any other error when it could not be reached.
   */
  dial(target: FabricTarget): Promise<{ url: string; path?: FabricPath }>;
  stop(): Promise<void>;
  /** The member's refusal of this phone since the last dial, or null. */
  refusal(): Promise<FabricRefusal | null>;
  now(): number;
  wait(ms: number): Promise<void>;
};

/** How long one member's dial may take before the walk moves on to the next member. */
export const DIAL_BUDGET_MS = 4000;
/** How long a whole walk down the list may take before the app uses its saved route instead. */
export const WALK_BUDGET_MS = 10_000;
/** After fabric failed, how long the next foreground goes straight to the saved route. */
export const REMEMBER_FAILURE_MS = 30_000;
/** After a failure the carrier tries fabric again by itself, after these waits (the last repeats): a phone that fell back to a saved route that cannot be reached must not stay there until it is killed. */
export const RETRY_WAITS_MS = [30_000, 60_000, 120_000, 300_000];
/** Waits before each of the redials after the feed lost a ready fabric connection, before the walk moves on. */
export const REDIAL_WAITS_MS = [1000, 2000, 5000];

const OFF: FabricSnapshot = { phase: 'off', bridgeUrl: '', path: 'unknown', reason: '', member: '', tried: 0 };

function reasonOf(error: unknown): string {
  return error instanceof Error && error.message ? error.message : 'Fabric could not connect';
}

/** A refusal in words: the member by its saved name and what fabric said, never a node ID or an address. */
export function refusalText(name: string, code: string | null): string {
  const what = code === 'peer_not_in_allow_list' ? `${name} refused this phone: it has no fabric grant for this phone.`
    : code === 'peer_not_permitted_for_service' ? `${name} refused this phone: its fabric grant for this phone does not include the client service.`
    : `${name} refused this phone's fabric access.`;
  return `${what} Members after it are not tried after a refusal; pick one to try it.`;
}

function sameMembers(a: FabricMember[], b: FabricMember[]): boolean {
  return a.length === b.length && a.every((m, i) => m.name === b[i].name && m.node === b[i].node && m.service === b[i].service && m.address === b[i].address);
}

type Outcome = 'ready' | 'refused' | 'stale' | { failed: string };

export class FabricCarrier {
  private snapshotValue: FabricSnapshot = OFF;
  private members: FabricMember[] = [];
  private enabled = false;
  private active = false;
  private epoch = 0;
  private redials = 0;
  private failedAt = -Infinity;
  private failedReason = '';
  private running = false;
  private failures = 0;
  /** The member in use. */
  private current: FabricMember | null = null;
  /** A member the person picked by hand (to use it, or to pair with it): only it is dialed until the list changes or the person asks for the list again. */
  private pinned: FabricMember | null = null;

  constructor(private readonly ports: CarrierPorts, private readonly changed: (snapshot: FabricSnapshot) => void) {}

  get snapshot(): FabricSnapshot { return this.snapshotValue; }

  private publish(next: Partial<FabricSnapshot>): void {
    this.snapshotValue = { ...this.snapshotValue, ...next };
    this.changed(this.snapshotValue);
  }

  /** The carrier choice, and the saved members in the person's order. */
  configure(enabled: boolean, members: FabricMember[]): void {
    if (enabled === this.enabled && sameMembers(members, this.members)) return;
    this.enabled = enabled;
    this.members = members.slice();
    this.failedAt = -Infinity;
    this.redials = 0;
    this.pinned = null;
    this.current = null;
    void this.reconcile(true);
  }

  foreground(active: boolean): void {
    if (active === this.active) return;
    this.active = active;
    void this.reconcile(false);
  }

  /** The person asks for fabric again after a failure or refusal: the list is walked from the top. */
  retry(): void { this.pinned = null; this.failedAt = -Infinity; this.redials = 0; this.failures = 0; void this.reconcile(true); }

  /** The person picks one member by hand: only that member is dialed, also one outside the walked list (to pair with it). */
  pick(member: FabricMember): void {
    this.pinned = member; this.failedAt = -Infinity; this.redials = 0; this.failures = 0;
    void this.reconcile(true);
  }

  /** The member picked by hand, if any. */
  get picked(): FabricMember | null { return this.pinned; }

  /** The feed over the bridge reports a good connection: redials start over. */
  feedLive(): void { this.redials = 0; this.failures = 0; }

  /**
   * The feed over the bridge lost its connection. A refusal ends the trial; anything else redials the
   * same member a few times, then walks on to the members after it.
   */
  async feedFailed(): Promise<void> {
    if (this.snapshotValue.phase !== 'ready') return;
    const epoch = this.epoch;
    let refusal: FabricRefusal | null = null;
    try { refusal = await this.ports.refusal(); } catch { /* a stats failure is not a refusal */ }
    if (epoch !== this.epoch) return;
    const member = this.current;
    this.epoch++;
    this.running = false;
    await this.ports.stop();
    if (!member) return;
    if (refusal) { this.refused(member, refusal.code); return; }
    await this.dropped(member);
  }

  /** The member was connected and is gone: redial it on the schedule, then move on to the members after it. */
  private async dropped(member: FabricMember): Promise<void> {
    const epoch = this.epoch;
    const name = member.name;
    if (this.redials >= REDIAL_WAITS_MS.length) {
      const at = this.members.findIndex(m => sameFabricTarget(m, member));
      const after = this.pinned === null && at >= 0 ? this.members.slice(at + 1) : [];
      if (after.length) { void this.walk(epoch, after); return; }
      this.failedAt = this.ports.now();
      this.failedReason = this.members.length > 1 ? `Fabric lost its connection to ${name}` : 'Fabric lost its connection to the member';
      this.publish({ phase: 'failed', bridgeUrl: '', reason: this.failedReason });
      this.scheduleRetry();
      return;
    }
    const wait = REDIAL_WAITS_MS[this.redials++];
    this.publish({ phase: 'dialing', bridgeUrl: '', reason: '', member: name });
    await this.ports.wait(wait);
    if (epoch !== this.epoch) return;
    void this.dial(epoch, member, DIAL_BUDGET_MS).then(outcome => {
      if (epoch === this.epoch && typeof outcome === 'object') void this.dropped(member);
    });
  }

  private async reconcile(restart: boolean): Promise<void> {
    const wanted = this.enabled && this.active && (this.members.length > 0 || !!this.pinned);
    if (!wanted) {
      const wasRunning = this.running;
      this.epoch++;
      this.running = false;
      if (wasRunning) await this.ports.stop();
      this.publish({ phase: this.enabled && (this.members.length || this.pinned) ? 'idle' : 'off', bridgeUrl: '', reason: '', member: '' });
      return;
    }
    if (this.running && !restart) return;
    await this.start();
  }

  private async start(): Promise<void> {
    if (!this.members.length && !this.pinned) return;
    const epoch = ++this.epoch;
    if (this.running) await this.ports.stop();
    if (epoch !== this.epoch) return;
    this.running = false;
    // A fabric that just failed is not waited on again at every foreground.
    if (this.ports.now() - this.failedAt < REMEMBER_FAILURE_MS) {
      this.publish({ phase: 'failed', bridgeUrl: '', reason: this.failedReason });
      return;
    }
    await this.walk(epoch, this.pinned ? [this.pinned] : this.members);
  }

  /** Dial the members in `order`, each within its own limit and all within the walk's, until one answers or one refuses. */
  private async walk(epoch: number, order: FabricMember[]): Promise<void> {
    const began = this.ports.now();
    let tried = 0;
    let last = '';
    this.publish({ phase: 'dialing', bridgeUrl: '', reason: '', member: '', tried: 0 });
    for (const member of order) {
      const left = WALK_BUDGET_MS - (this.ports.now() - began);
      if (left <= 0) break;
      tried++;
      this.publish({ member: member.name, tried });
      const outcome = await this.dial(epoch, member, Math.min(DIAL_BUDGET_MS, left));
      if (typeof outcome !== 'object') return;
      last = outcome.failed;
    }
    if (epoch !== this.epoch) return;
    this.failedAt = this.ports.now();
    this.failedReason = this.members.length <= 1 && !this.pinned ? last
      : tried < order.length ? `No member answered over fabric within ${Math.round(WALK_BUDGET_MS / 1000)} s (${tried} of ${order.length} tried)`
      : tried === 1 ? `${order[0].name}: ${last}`
      : `None of the ${tried} members answered over fabric`;
    this.publish({ phase: 'failed', bridgeUrl: '', reason: this.failedReason, member: '' });
    this.scheduleRetry();
  }

  /** One member's dial within `budgetMs`. */
  private async dial(epoch: number, member: FabricMember, budgetMs: number): Promise<Outcome> {
    const budget = this.ports.wait(budgetMs).then(() => 'late' as const);
    try {
      this.running = true;
      const outcome = await Promise.race([this.ports.dial(member), budget]);
      if (epoch !== this.epoch) { if (outcome !== 'late') await this.ports.stop(); return 'stale'; }
      if (outcome === 'late') {
        this.running = false;
        await this.ports.stop();
        return { failed: `Fabric did not answer within ${Math.round(budgetMs / 1000)} s` };
      }
      this.current = member;
      this.publish({ phase: 'ready', bridgeUrl: outcome.url, path: outcome.path ?? 'unknown', reason: '', member: member.name });
      return 'ready';
    } catch (error) {
      if (epoch !== this.epoch) return 'stale';
      this.running = false;
      await this.ports.stop().catch(() => {});
      if (error instanceof FabricRefusal) { this.refused(member, error.code); return 'refused'; }
      return { failed: reasonOf(error) };
    }
  }

  /** A refusal stops everything: nothing else is dialed, and nothing is tried again by itself. */
  private refused(member: FabricMember, code: string | null): void {
    this.publish({ phase: 'refused', bridgeUrl: '', reason: refusalText(member.name, code), member: member.name });
  }

  /** Try fabric again after a wait that grows, while it is still wanted and still failed. */
  private scheduleRetry(): void {
    const epoch = this.epoch;
    const wait = RETRY_WAITS_MS[Math.min(this.failures++, RETRY_WAITS_MS.length - 1)];
    void this.ports.wait(wait).then(() => {
      if (epoch !== this.epoch || this.snapshotValue.phase !== 'failed') return;
      void this.reconcile(true);
    });
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
    const why = fallback ? 'No fabric target is saved, so the saved gateway is used' : 'No fabric target is saved (fabric only, no fallback)';
    return fallback && saved ? { baseUrl: saved, route: savedRoute(saved, true, why), pending: false, issue: '' } : { baseUrl: null, route: { kind: 'none', fellBack: false, why: '' }, pending: false, issue: why };
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
