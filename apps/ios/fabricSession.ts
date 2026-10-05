// The development session owns only in-memory credentials and native-created listeners.
// Pairing is attempted once. Resuming opens a fresh listener and repeats only a read.
export type ProofEvent = { atMs: number; event: string; fields?: Record<string, string | number | boolean> };
export type ProofState = { url: string; credential: string; actor: string; ready: boolean; issue: string; events: ProofEvent[] };
type Ports = {
  dial(): Promise<{ url: string }>;
  pair(url: string): Promise<string>;
  authenticate(url: string, credential: string): Promise<string>;
  stop(): Promise<void>;
};

export class FabricSession {
  private state: ProofState = { url: '', credential: '', actor: '', ready: false, issue: '', events: [] };
  private epoch = 0;
  private active = false;
  private closed = false;
  private pairingAttempted = false;
  private refused = false;
  private opening: Promise<void> = Promise.resolve();
  private readonly began = performance.now();

  constructor(private readonly ports: Ports, private readonly changed: (state: ProofState) => void) {}

  record = (event: string, fields?: ProofEvent['fields']): void => {
    this.state.events = [...this.state.events.slice(-127), { atMs: performance.now() - this.began, event, ...(fields ? { fields } : {}) }];
    this.publish();
  };

  private publish(): void { if (!this.closed) this.changed({ ...this.state }); }

  async foreground(active: boolean): Promise<void> {
    if (this.closed || active === this.active) return;
    this.active = active;
    const epoch = ++this.epoch;
    this.state.ready = false;
    this.record(active ? 'foreground' : 'background');
    if (!active) { await this.ports.stop(); return; }
    if (this.refused) return;
    // Wait for an interrupted pairing response before deciding whether there is a credential.
    this.opening = this.opening.then(async () => {
      if (this.closed || !this.active || epoch !== this.epoch) return;
      const began = performance.now();
      try {
        const bridge = await this.ports.dial();
        if (this.closed || !this.active || epoch !== this.epoch) { await this.ports.stop(); return; }
        this.state.url = bridge.url;
        if (!this.state.credential) {
          if (this.pairingAttempted) throw new Error('Pairing was interrupted. Open a fresh proof pairing link.');
          this.pairingAttempted = true;
          // Retain a received credential even if backgrounding invalidated this listener.
          const credential = await this.ports.pair(bridge.url);
          if (!this.closed) this.state.credential = credential;
        }
        if (this.closed || !this.active || epoch !== this.epoch) return;
        const actor = await this.ports.authenticate(bridge.url, this.state.credential);
        if (this.closed || !this.active || epoch !== this.epoch) return;
        if (this.state.actor && actor !== this.state.actor) throw new Error('The proof session actor changed. Open a fresh proof link.');
        this.state.actor = actor;
        this.state.ready = true;
        this.state.issue = '';
        this.record('authenticated', { elapsedMs: performance.now() - began });
      } catch (error) {
        if (this.closed || epoch !== this.epoch) return;
        this.state.issue = error instanceof Error ? error.message : 'Fabric connection failed';
        this.state.ready = false;
        this.record('connection-failed');
        await this.ports.stop();
      }
    });
    await this.opening;
  }

  async close(): Promise<void> {
    this.closed = true; this.active = false; this.epoch++;
    this.state.credential = ''; this.state.ready = false;
    await this.ports.stop();
  }

  async refuse(): Promise<void> {
    if (this.closed) return;
    this.epoch++; this.refused = true; this.state.ready = false;
    this.state.issue = 'The member refused fabric access. Check the phone grant and open a fresh proof link.';
    this.record('admission-refused');
    await this.ports.stop();
  }
}

/** Old refusals from a previous trial cannot poison a newly granted session. */
export function nativeRefusedAfter(stats: Record<string, unknown>, floor: number): boolean {
  return Array.isArray(stats.attempts) && stats.attempts.some(a => a && typeof a.id === 'number' && a.id > floor && a.result === 'refused');
}
