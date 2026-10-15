import type { CollectionStream, Snapshot, St3Client, TerminalRoute, TerminalScreen } from '../../clients/typescript/st3-client';
import { plainError } from '../../clients/typescript/st3-client';
import { daemonRouteLabel, fabricRouteLabel, terminalRoute, TerminalRouteMemory } from '@smalltalk/st3-views';
import { TERMINAL_RESTARTED, withFreshTerminalFence, type TerminalFollowHandlers } from './terminalControls';
import type { HeartbeatSocketFactory } from './collectionSocket';

export type TerminalClient = Pick<St3Client, 'capabilities' | 'terminalScreen' | 'terminalAttach' | 'terminalDetach' | 'terminalInput' | 'terminalResize' | 'collectionStream'>;
export type OwnerTarget = {
  client: TerminalClient;
  /** Detach through this issuer even after the attempt's HTTP reads were cancelled. */
  cleanup: TerminalClient;
  release(after?: Promise<void>): Promise<void>;
  failure(error: unknown): Promise<{ reason: string; unreachable: boolean }>;
};
export type TerminalRouting = {
  fallback(): boolean;
  built(): boolean;
  describe(id: string, signal: AbortSignal): Promise<TerminalRoute>;
  open(route: TerminalRoute, signal: AbortSignal): Promise<OwnerTarget>;
  memory: TerminalRouteMemory;
  attemptMs?: number;
};
const code = (error: unknown): string | undefined => (error as { response?: { code?: string } })?.response?.code;
const restarted = (error: unknown) => error instanceof Error && error.message === TERMINAL_RESTARTED;

/** One followed terminal, independent of the gateway windows. Every transport holds one pin. */
export class RoutedTerminalFollow {
  private pin = '';
  private generation = 0;
  private closed = false;
  private controller?: AbortController;
  private target?: OwnerTarget;
  private socket?: CollectionStream;
  private current?: TerminalClient;
  private snapshot = '';
  private direct = false;
  private attached?: { client: TerminalClient; id: string };
  private descriptor?: TerminalRoute;
  private starting = false;
  private timer?: ReturnType<typeof setTimeout>;
  private failures = 0;
  private routeReason = '';
  private qualityAttached = false;
  private first?: { resolve(): void; reject(error: Error): void };

  constructor(private readonly gateway: TerminalClient, private readonly id: string,
    private readonly handlers: TerminalFollowHandlers, private readonly actionId: () => string,
    private readonly gatewaySocket: () => CollectionStream | undefined,
    private readonly routing: TerminalRouting, private readonly active: () => boolean,
    private readonly released: () => void, private readonly retries: readonly number[],
    private readonly socketFactory?: HeartbeatSocketFactory) { this.pin = handlers.expectedIncarnation ?? ''; }

  async attach(): Promise<void> {
    if (this.closed || this.starting || this.direct || !this.active()) return;
    this.starting = true;
    clearTimeout(this.timer);
    const generation = ++this.generation;
    this.dispose();
    this.descriptor = undefined;
    const controller = new AbortController();
    this.controller = controller;
    const valid = () => !this.closed && generation === this.generation && this.active();
    let deadline: ReturnType<typeof setTimeout> | undefined;
    let remembered = false;
    let timedOut = false;
    try {
      await Promise.race([
        (async () => {
          if (!this.routing.built()) throw new Error('This build has no fabric bridge');
          const route = terminalRoute(await this.routing.describe(this.id, controller.signal), this.id, this.pin);
          if (!valid()) return;
          this.pin ||= route.runtime_incarnation;
          this.descriptor = route;
          if (!route.fabric) throw new Error('The current owner advertises no Fabric client service');
          const previous = this.routing.memory.reason(route);
          if (previous) { remembered = true; throw new Error(previous); }
          const target = await this.routing.open(route, controller.signal);
          if (!valid() || controller.signal.aborted) { await target.release(); return; }
          this.target = target;
          this.current = target.client;
          // Validate the owner's first read before obtaining an owner-local attachment.
          const screen = await target.client.terminalScreen(this.id);
          if (!valid() || controller.signal.aborted) return;
          this.check(screen.value);
          this.snapshot = screen.snapshot.id;
          const capabilities = await target.client.capabilities();
          if (!valid() || controller.signal.aborted) return;
          const canControl = capabilities.value.capabilities.some(
            capability => capability.id === 'terminal.input' && capability.state === 'granted');
          const attachment = await this.obtain(target.client);
          if (!valid() || controller.signal.aborted) { void this.detach(target.cleanup, attachment.id); return; }
          this.attached = { client: target.cleanup, id: attachment.id };
          this.qualityAttached = true;
          this.handlers.onQuality?.('attached');
          const first = new Promise<void>((resolve, reject) => { this.first = { resolve, reject }; });
          void first.catch(() => {}); // A close/error can precede the socket-open promise.
          const socket = await target.client.collectionStream({
            ...(this.socketFactory ? { socket: this.socketFactory } : {}),
            onFrame: frame => {
              if (!valid() || controller.signal.aborted || frame.id !== 'terminal') return;
              if (frame.kind === 'screen' && frame.collection === 'terminal') {
                try { this.check(frame.value); } catch (error) { this.first?.reject(error as Error); this.stop(plainError(error)); return; }
                this.snapshot = frame.snapshot.id;
                this.direct = true;
                this.handlers.onControl?.(canControl);
                this.handlers.onReady?.(true);
                this.handlers.onRoute?.(fabricRouteLabel(route));
                this.handlers.onIssue('');
                this.handlers.onScreen(frame.value);
                this.first?.resolve(); this.first = undefined;
              } else if (frame.kind === 'error') {
                const error = Object.assign(new Error(frame.message), { response: { code: frame.code } });
                if (this.first) this.first.reject(error);
                else void this.ownerFailed(error);
              }
            },
            onEnd: error => {
              if (!valid() || controller.signal.aborted) return;
              const ended = error ?? new Error('The owner terminal connection closed');
              if (this.first) this.first.reject(ended);
              else void this.ownerFailed(ended);
            },
          });
          if (!valid() || controller.signal.aborted) { socket.close(); return; }
          this.socket = socket;
          socket.subscribeTerminal('terminal', this.id, this.pin, attachment.capability);
          await first;
          if (valid()) this.routing.memory.succeeded(route);
        })(),
        new Promise<never>((_, reject) => { deadline = setTimeout(() => { timedOut = true; reject(new Error(`Fabric did not answer within ${(this.routing.attemptMs ?? 4000) / 1000} s`)); }, this.routing.attemptMs ?? 4000); }),
      ]);
    } catch (error) {
      if (!valid()) return;
      if (restarted(error)) { this.stop(TERMINAL_RESTARTED); return; }
      controller.abort();
      const failure = remembered || timedOut ? { reason: plainError(error), unreachable: timedOut && !!this.target }
        : await this.failure(error);
      if (!valid()) return;
      if (!remembered && failure.unreachable && this.descriptor) this.routing.memory.failed(this.descriptor, failure.reason);
      this.routeReason = daemonRouteLabel(failure.reason, remembered);
      this.dispose();
      if (!this.routing.fallback()) { this.stop(`Fabric: ${failure.reason}${remembered ? ', not tried again yet' : ''} (fabric only, no fallback)`); return; }
      this.handlers.onRoute?.(this.routeReason);
      await this.attachGateway(valid);
    } finally {
      clearTimeout(deadline);
      if (generation === this.generation) this.starting = false;
    }
  }

  private check(screen: TerminalScreen): void {
    if (screen.runtime_incarnation !== this.pin) throw new Error(TERMINAL_RESTARTED);
    if (screen.terminal_id && screen.terminal_id.replace(/^terminal\//, '') !== this.id.replace(/^terminal\//, '')) throw new Error('The owner returned another terminal');
  }

  private async obtain(client: TerminalClient): Promise<{ id: string; capability: string }> {
    this.handlers.onQuality?.('attempt');
    try {
      const result = await withFreshTerminalFence(client, this.id, this.pin, fence => {
        const id = this.actionId();
        return client.terminalAttach({ id, idempotency_key: id, fence, parameters: { target_id: this.id } });
      });
      const attachment = result.value.terminal_attachment;
      if (!attachment?.stream_capability || !attachment.attachment_id) throw new Error('The terminal issuer returned no attachment or stream capability');
      if (attachment.runtime_incarnation && attachment.runtime_incarnation !== this.pin) throw new Error(TERMINAL_RESTARTED);
      return { id: attachment.attachment_id, capability: attachment.stream_capability };
    } catch (error) {
      if (!this.closed && this.active() && this.current === client) this.handlers.onQuality?.('error');
      throw error;
    }
  }

  private async attachGateway(valid: () => boolean): Promise<void> {
    try {
      const screen = await this.gateway.terminalScreen(this.id);
      if (!valid()) return;
      this.pin ||= screen.value.runtime_incarnation;
      this.check(screen.value);
      this.snapshot = screen.snapshot.id;
      this.current = this.gateway;
      this.handlers.onScreen(screen.value);
      const attachment = await this.obtain(this.gateway);
      if (!valid()) { void this.detach(this.gateway, attachment.id); return; }
      this.attached = { client: this.gateway, id: attachment.id };
      this.qualityAttached = true;
      this.handlers.onQuality?.('attached');
      this.gatewaySocket()?.subscribeTerminal('terminal', this.id, this.pin, attachment.capability);
      this.handlers.onIssue('');
    } catch (error) {
      if (!valid()) return;
      if (restarted(error)) this.stop(TERMINAL_RESTARTED);
      else if (code(error) && !['stale-fence', 'remote-unavailable'].includes(code(error)!)) this.stop(`${this.routeReason}: ${plainError(error)}`);
      else this.retry(`${this.routeReason}: ${plainError(error)}`);
    }
  }

  screen(screen: TerminalScreen, snapshot?: Snapshot): void {
    if (this.closed || this.direct || this.current !== this.gateway) return;
    try { this.check(screen); } catch { this.stop(TERMINAL_RESTARTED); return; }
    if (snapshot) this.snapshot = snapshot.id;
    this.failures = 0;
    this.handlers.onReady?.(true);
    this.handlers.onIssue('');
    this.handlers.onScreen(screen);
  }

  failed(code: string | undefined, message: string): void {
    if (this.closed || this.direct || this.current !== this.gateway) return;
    if (code && !['stale-fence', 'remote-unavailable'].includes(code)) this.stop(`${this.routeReason}: ${message}`);
    else this.retry(message);
  }

  socketLost(unexpected = false): void {
    // Gateway recovery does not close a healthy owner stream. Background closes both routes.
    if (this.active() && this.direct) return;
    if (unexpected) this.dead();
    ++this.generation;
    this.starting = false;
    this.dispose();
  }

  control(id: string, incarnation: string): { client: TerminalClient; snapshot: string } {
    if (this.closed || !this.current || !this.snapshot || !this.active() || !this.direct && (this.current !== this.gateway || !this.attached)) throw new Error('The terminal route is not ready; keys were not sent');
    if (id !== this.id) throw new Error('This terminal is not the followed route; keys were not sent');
    if (incarnation !== this.pin) throw new Error(TERMINAL_RESTARTED);
    return { client: this.current, snapshot: this.snapshot };
  }

  async controlFailed(error: unknown): Promise<void> { if (this.direct) await this.ownerFailed(error); }

  private async failure(error: unknown): Promise<{ reason: string; unreachable: boolean }> {
    if (this.target) return this.target.failure(error);
    return { reason: plainError(error), unreachable: (error as { ownerUnreachable?: boolean })?.ownerUnreachable !== false && !code(error) && !!this.descriptor?.fabric };
  }

  private async ownerFailed(error: unknown): Promise<void> {
    if (this.closed || !this.direct) return;
    this.dead();
    if (restarted(error)) { this.stop(TERMINAL_RESTARTED); return; }
    const generation = ++this.generation;
    this.direct = false; this.snapshot = '';
    this.handlers.onReady?.(false);
    const failure = await this.failure(error);
    if (generation !== this.generation || this.closed) return;
    if (failure.unreachable && this.descriptor) this.routing.memory.failed(this.descriptor, failure.reason);
    this.routeReason = daemonRouteLabel(failure.reason);
    this.dispose();
    if (!this.routing.fallback()) { this.stop(`Fabric: ${failure.reason} (fabric only, no fallback)`); return; }
    this.handlers.onRoute?.(this.routeReason);
    await this.attachGateway(() => !this.closed && generation === this.generation && this.active());
  }

  private retry(issue: string): void {
    if (this.closed) return;
    this.dead();
    this.handlers.onIssue(`Terminal stream interrupted; reconnecting (${issue}).`);
    clearTimeout(this.timer);
    this.timer = setTimeout(() => { void this.attach(); }, this.retries[Math.min(this.failures++, this.retries.length - 1)]);
  }

  private detach(client: TerminalClient, attachmentId: string): Promise<void> {
    return withFreshTerminalFence(client, this.id, this.pin, fence => {
      const id = this.actionId();
      return client.terminalDetach({ id, idempotency_key: id, fence, parameters: { target_id: attachmentId } });
    }).then(() => {}, () => {});
  }

  private dispose(): void {
    this.qualityAttached = false;
    this.handlers.onControl?.(undefined);
    this.handlers.onReady?.(false);
    this.controller?.abort(); this.controller = undefined;
    this.first?.reject(new Error('Terminal route cancelled')); this.first = undefined;
    this.socket?.unsubscribe('terminal'); this.socket?.close(); this.socket = undefined;
    if (!this.direct) this.gatewaySocket()?.unsubscribe('terminal');
    const issuer = this.attached, target = this.target;
    this.attached = undefined; this.target = undefined; this.current = undefined; this.snapshot = ''; this.direct = false;
    // Release even if detach is refused or the network closes; the native target is bounded.
    if (target) void target.release(issuer ? this.detach(issuer.client, issuer.id) : undefined).catch(() => {});
    else if (issuer) void this.detach(issuer.client, issuer.id);
  }

  private stop(issue: string): void {
    if (issue) this.dead();
    this.closed = true;
    ++this.generation;
    clearTimeout(this.timer);
    this.dispose();
    this.released();
    if (issue) this.handlers.onIssue(issue);
  }
  close(): void { if (!this.closed) this.stop(''); }
  private dead(): void {
    if (this.qualityAttached) { this.qualityAttached = false; this.handlers.onQuality?.('dead'); }
  }
}
