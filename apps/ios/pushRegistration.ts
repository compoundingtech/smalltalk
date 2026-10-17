import type { PushRegistration, PushRegistrationStatus } from '../../clients/typescript/st3-client';

export type PushEnvironment = PushRegistration['environment'];
export type PushDecision = 'none' | 'register' | 'unregister';
export type PushEligibility = { paired: boolean; granted: boolean; enabled: boolean; available: boolean; permission: boolean };

/** Disabling (or losing permission) removes the member's registration, even on a non-push build. */
export const pushDecision = (state: PushEligibility): PushDecision => {
  if (!state.paired || !state.granted) return 'none';
  if (!state.enabled || !state.permission) return 'unregister';
  return state.available ? 'register' : 'none';
};

/** Native input is never echoed into errors. Tokens stay in the request, not app state/storage. */
export const pushRequest = (token: unknown, environment: PushEnvironment, bundleId: string | null, version: string | null): PushRegistration | undefined => {
  if (typeof token !== 'string' || !/^(?:[a-fA-F0-9]{2}){1,256}$/.test(token)) return;
  if (!bundleId || !/^[A-Za-z0-9.-]{1,255}$/.test(bundleId)) return;
  if (!version || !/^[\x20-\x7e]{1,64}$/.test(version)) return;
  return { token, environment, bundle_id: bundleId, app_version: version };
};

export type PushRemote = {
  register(request: PushRegistration): Promise<PushRegistrationStatus>;
  unregister(): Promise<PushRegistrationStatus>;
};
export type PushContext = {
  paired: boolean;
  granted: boolean;
  issuer?: string;
  remote?: PushRemote;
  /** Saved paired profile identities, with only routes currently safe for their own credentials. */
  issuers: ReadonlyMap<string, PushRemote | undefined>;
};
export type PushState = {
  enabled: boolean;
  loaded: boolean;
  environment?: PushEnvironment;
  cleanup: 'none' | 'pending' | 'failed' | 'complete';
  status: 'idle' | 'working' | 'registered' | 'unregistered' | 'permission-denied' | 'failed';
};
export type PushPorts = {
  readEnabled(): Promise<boolean>;
  writeEnabled(enabled: boolean): Promise<void>;
  environment(): Promise<PushEnvironment | undefined>;
  permission(request: boolean): Promise<boolean>;
  token(): Promise<unknown>;
  listen(changed: (token: unknown) => void): () => void;
  bundleId: string | null;
  version: string | null;
};

/** One phone-local lifecycle. Serialize writes so opt-out follows an already-sent register.
 * Native acquisition is generation-fenced; failed transport writes reconcile on the next connection.
 */
export class PushRegistrationLifecycle {
  private state: PushState = { enabled: false, loaded: false, status: 'idle', cleanup: 'none' };
  private context: PushContext = { paired: false, granted: false, issuers: new Map() };
  private listeners = new Set<() => void>();
  private owners = new Set<string>();
  private ownershipReady = false;
  private retiring = new Set<string>();
  private stopTokens?: () => void;
  private loading?: Promise<void>;
  private queue: Promise<void> = Promise.resolve();
  private generation = 0;
  private nativeWaiters = new Set<() => void>();
  private forgetting = false;
  private disposed = false;

  constructor(private readonly ports: PushPorts, private readonly ownership: {
    read(): Promise<readonly string[]>;
    write(issuers: readonly string[]): Promise<void>;
  }) {}

  private invalidate(): void {
    this.generation++;
    for (const cancel of this.nativeWaiters) cancel();
    this.nativeWaiters.clear();
  }
  /** Native APNs acquisition can wait indefinitely. Opt-out/forget must not wait for that callback. */
  private async readNative<T>(read: () => Promise<T>, generation: number): Promise<T | undefined> {
    if (generation !== this.generation) return;
    let cancel = () => {};
    const canceled = new Promise<undefined>(resolve => { cancel = () => resolve(undefined); });
    this.nativeWaiters.add(cancel);
    try { return await Promise.race([read(), canceled]); }
    finally { this.nativeWaiters.delete(cancel); }
  }

  readonly snapshot = (): PushState => this.state;
  readonly subscribe = (changed: () => void): (() => void) => {
    this.listeners.add(changed);
    return () => { this.listeners.delete(changed); };
  };
  private publish(patch: Partial<PushState>): void {
    this.state = { ...this.state, ...patch };
    for (const changed of this.listeners) changed();
  }
  private enqueue(job: () => Promise<void>): Promise<void> {
    this.queue = this.queue.then(job).catch(() => {
      if (!this.disposed) this.publish({ status: 'failed', cleanup: this.forgetting || !this.state.enabled ? 'failed' : this.state.cleanup });
    });
    return this.queue;
  }
  private eligible(): boolean {
    return !this.disposed && !this.forgetting && this.ownershipReady && !this.retiring.has(this.context.issuer ?? '') && this.state.loaded && this.context.paired && this.context.granted && !!this.context.remote;
  }
  private listen(): void {
    this.stopTokens?.(); this.stopTokens = undefined;
    if (!this.eligible() || !this.state.enabled || !this.state.environment) return;
    const generation = this.generation;
    this.stopTokens = this.ports.listen(token => {
      if (generation === this.generation) void this.enqueue(() => this.reconcile(false, token));
    });
  }
  start(): Promise<void> {
    this.disposed = false;
    if (!this.loading) this.loading = (async () => {
      const [enabled, environment, owners] = await Promise.all([this.ports.readEnabled(), this.ports.environment(), this.ownership.read()]);
      this.owners = new Set(owners);
      this.ownershipReady = true;
      this.publish({ enabled, environment, loaded: true });
    })().catch(() => { this.loading = undefined; this.publish({ loaded: true, status: 'failed', cleanup: 'failed' }); });
    return this.loading.then(() => {
      if (this.disposed) return;
      this.listen();
      return this.refresh();
    });
  }
  configure(context: PushContext): void {
    this.invalidate();
    this.context = context;
    for (const issuer of this.retiring) if (!context.issuers.has(issuer)) this.retiring.delete(issuer);
    this.listen();
    if (this.state.loaded) void this.refresh();
  }
  stop(): void {
    this.disposed = true; this.invalidate();
    this.stopTokens?.(); this.stopTokens = undefined;
  }
  refresh(): Promise<void> { return this.enqueue(() => this.reconcile(false)); }

  /** Only this intentional action can request iOS permission. Enabling cannot happen unpaired. */
  setEnabled(enabled: boolean): Promise<void> {
    if (enabled && (!this.eligible() || !this.state.environment)) return Promise.resolve();
    this.invalidate();
    this.publish({ enabled, status: 'working' });
    this.listen();
    const generation = this.generation;
    return this.enqueue(async () => {
      await this.ports.writeEnabled(enabled);
      if (generation !== this.generation) return;
      await this.reconcile(enabled);
    });
  }
  private async reconcile(requestPermission: boolean, changedToken?: unknown): Promise<void> {
    if (!this.state.enabled) {
      if (!this.disposed) await this.cleanup();
      return;
    }
    const remote = this.context.remote;
    if (!this.eligible() || !remote) return;
    const generation = this.generation;
    const current = () => this.eligible() && generation === this.generation;
    this.publish({ status: 'working' });
    const permission = this.state.enabled && this.state.environment ? await this.readNative(() => this.ports.permission(requestPermission), generation) : false;
    if (!current()) return;
    const decision = pushDecision({ paired: this.context.paired, granted: this.context.granted, enabled: this.state.enabled, available: !!this.state.environment, permission: permission === true });
    if (decision === 'unregister') {
      await this.cleanup();
      if (current() && this.state.cleanup === 'complete') this.publish({ status: 'permission-denied' });
      return;
    }
    if (decision !== 'register' || !this.state.environment) return;
    if (!this.context.issuer) return;
    // Journal only the safe profile identity BEFORE sending: a lost response may still register.
    const issuer = this.context.issuer;
    this.owners.add(issuer);
    await this.ownership.write([...this.owners]);
    if (!current()) return;
    const token = changedToken ?? await this.readNative(() => this.ports.token(), generation);
    if (!current()) return;
    const request = pushRequest(token, this.state.environment, this.ports.bundleId, this.ports.version);
    if (!request) { this.publish({ status: 'failed' }); return; }
    this.publish({ cleanup: 'none' });
    const result = await remote.register(request);
    if (current()) this.publish({ status: result.registered ? 'registered' : 'unregistered' });
  }

  /** Persisted profile identities own cleanup; tokens and credentials never enter the journal. */
  private async cleanup(only?: string): Promise<boolean> {
    await this.loading;
    if (!this.ownershipReady) { this.publish({ cleanup: 'failed', status: 'failed' }); return false; }
    let failed = false;
    let pending = false;
    this.publish({ cleanup: 'pending', status: 'working' });
    for (const issuer of [...this.owners]) {
      if (only !== undefined && issuer !== only) continue;
      const remote = this.context.issuers.get(issuer);
      if (!remote) { pending = true; continue; }
      try {
        const result = await remote.unregister();
        if (result.registered) { failed = true; continue; }
        const remaining = [...this.owners].filter(owner => owner !== issuer);
        await this.ownership.write(remaining);
        this.owners.delete(issuer);
      } catch { failed = true; }
    }
    const complete = !failed && !pending;
    const outstanding = pending || (!this.state.enabled && this.owners.size > 0);
    this.publish({ cleanup: failed ? 'failed' : outstanding ? 'pending' : 'complete', status: complete && !outstanding ? 'unregistered' : 'failed' });
    return complete;
  }

  /** Fence native work, then clean all issuers after already-sent registrations settle.
   * Failure retains pairing credentials so reconnect + retry can finish remote opt-out.
   */
  async forget(): Promise<boolean> {
    this.forgetting = true; this.invalidate();
    this.stopTokens?.(); this.stopTokens = undefined;
    let complete = false;
    await this.enqueue(async () => {
      await this.ports.writeEnabled(false);
      this.publish({ enabled: false });
      complete = await this.cleanup();
    });
    if (!complete) this.forgetting = false;
    return complete;
  }

  /** Removing an independent member must finish cleanup before its credential can be deleted. */
  async forgetIssuer(issuer: string): Promise<boolean> {
    this.invalidate();
    this.retiring.add(issuer);
    let complete = false;
    await this.enqueue(async () => { complete = await this.cleanup(issuer); });
    if (!complete) { this.retiring.delete(issuer); this.listen(); }
    return complete;
  }
  releaseIssuer(issuer: string): void { this.retiring.delete(issuer); }
  resume(): void { this.forgetting = false; }
}
