export type ImageRect = { x: number; y: number; width: number; height: number };
type MeasuredImage = { measure: (done: (rect: ImageRect) => void) => void; visible: (shown: boolean) => void };

/** Native measurements, not FlatList's overscan/mount window, authorize image reads. */
export class ConversationImageViewport {
  private images = new Set<MeasuredImage>();
  private bounds?: ImageRect;
  private active = false;
  private generation = 0;
  private reads = new Set<() => Promise<void>>();
  private reading = false;

  register(image: MeasuredImage): () => void {
    this.images.add(image);
    this.refresh();
    return () => { this.images.delete(image); image.visible(false); };
  }

  setBounds(bounds: ImageRect): void { this.bounds = bounds; this.refresh(); }
  setActive(active: boolean): void { this.active = active; this.refresh(); }

  /** Hold one viewport-wide slot through every chunk, including an offscreen pending request. */
  enqueue(read: () => Promise<void>): () => void {
    this.reads.add(read);
    void this.drain();
    return () => { this.reads.delete(read); };
  }

  private async drain(): Promise<void> {
    if (this.reading) return;
    this.reading = true;
    try {
      for (const read of this.reads) {
        this.reads.delete(read);
        await read();
      }
    } finally {
      this.reading = false;
    }
  }

  refresh(): void {
    const generation = ++this.generation;
    const bounds = this.bounds;
    for (const image of this.images) {
      if (!this.active || !bounds) { image.visible(false); continue; }
      image.measure(rect => {
        if (generation !== this.generation || !this.images.has(image)) return;
        image.visible(rect.width > 0 && rect.height > 0 && bounds.width > 0 && bounds.height > 0
          && rect.x < bounds.x + bounds.width && rect.x + rect.width > bounds.x
          && rect.y < bounds.y + bounds.height && rect.y + rect.height > bounds.y);
      });
    }
  }
}

export type VisibleImageState<TImage> = { kind: 'closed' } | { kind: 'loading' } | { kind: 'failed'; message: string } | { kind: 'image'; image: TImage };

type PendingImageRead = { generation: number; phase: 'queued' | 'reading'; cancelQueued?: () => void };

/** Component-owned state; the viewport, not each image, owns the sequential read queue.
 * The client cannot abort a content request. Reentry reuses it until a chunk boundary
 * observes invisibility, and cancellation keeps the queue slot until the load settles. */
export class VisibleConversationImage<TImage> {
  private state: VisibleImageState<TImage> = { kind: 'closed' };
  private visible = false;
  private generation = 0;
  private disposed = false;
  private pending?: PendingImageRead;

  constructor(
    private viewport: ConversationImageViewport,
    private load: (current: () => boolean) => Promise<TImage>,
    private changed: (state: VisibleImageState<TImage>) => void,
  ) {}

  setVisible(visible: boolean): void {
    if (this.disposed) return;
    this.visible = visible;
    if (!visible) {
      if (this.pending?.phase === 'queued') this.cancel();
      if (this.state.kind === 'loading') this.publish({ kind: 'closed' });
    } else if (this.state.kind === 'closed') {
      if (this.pending) this.publish({ kind: 'loading' });
      else this.enqueue();
    }
  }

  retry(): void { if (this.visible && this.state.kind === 'failed') this.enqueue(); }
  fail(message: string): void { if (!this.disposed) { this.cancel(); this.publish({ kind: 'failed', message }); } }
  dispose(): void { this.disposed = true; this.cancel(); this.state = { kind: 'closed' }; }

  private cancel(): void {
    this.generation++;
    this.pending?.cancelQueued?.();
    this.pending = undefined;
  }

  private publish(state: VisibleImageState<TImage>): void { this.state = state; this.changed(state); }

  private enqueue(): void {
    const pending: PendingImageRead = { generation: ++this.generation, phase: 'queued' };
    this.pending = pending;
    this.publish({ kind: 'loading' });
    if (this.pending !== pending || this.disposed || !this.visible) return;
    const cancelQueued = this.viewport.enqueue(() => this.read(pending));
    if (pending.phase === 'queued') pending.cancelQueued = cancelQueued;
  }

  private async read(pending: PendingImageRead): Promise<void> {
    pending.phase = 'reading';
    pending.cancelQueued = undefined;
    if (this.pending !== pending || this.disposed || !this.visible) return;
    let revoked = false;
    const current = () => {
      if (revoked || this.disposed || this.generation !== pending.generation) return false;
      if (!this.visible) { revoked = true; return false; }
      return true;
    };
    try {
      const image = await this.load(current);
      if (current()) this.publish({ kind: 'image', image });
    } catch (error) {
      // Owner failures remain explicit-retry failures even if the image went offscreen.
      if (!revoked && !this.disposed && this.generation === pending.generation) {
        this.publish({ kind: 'failed', message: error instanceof Error ? error.message : String(error) });
      }
    } finally {
      if (this.pending === pending) {
        this.pending = undefined;
        if (revoked) {
          if (this.state.kind !== 'closed') this.publish({ kind: 'closed' });
          if (this.visible) this.enqueue();
        }
      }
    }
  }
}
