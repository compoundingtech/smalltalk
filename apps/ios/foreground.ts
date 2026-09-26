// Loops that follow the gateway must stop while iOS has the app in the background. Waiting for the
// foreground is event-driven: no timer or poll runs until AppState reports `active` again.
export class ForegroundGate {
  private isActive: boolean;
  private waiters: Array<() => void> = [];
  private listeners = new Set<(active: boolean) => void>();

  constructor(initialState: string | null | undefined) { this.isActive = initialState === 'active'; }

  get active(): boolean { return this.isActive; }

  update(state: string | null | undefined): void {
    const active = state === 'active';
    if (active === this.isActive) return;
    this.isActive = active;
    for (const listener of this.listeners) listener(active);
    if (active) { const waiters = this.waiters; this.waiters = []; for (const resume of waiters) resume(); }
  }

  untilActive(): Promise<void> {
    return this.isActive ? Promise.resolve() : new Promise(resolve => { this.waiters.push(resolve); });
  }

  subscribe(listener: (active: boolean) => void): () => void {
    this.listeners.add(listener);
    return () => { this.listeners.delete(listener); };
  }
}
