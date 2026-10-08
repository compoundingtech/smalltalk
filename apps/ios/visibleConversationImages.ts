export type ImageRect = { x: number; y: number; width: number; height: number };
type MeasuredImage = { measure: (done: (rect: ImageRect) => void) => void; visible: (shown: boolean) => void };

/** Native measurements, not FlatList's overscan/mount window, authorize image reads. */
export class ConversationImageViewport {
  private images = new Set<MeasuredImage>();
  private bounds?: ImageRect;
  private active = false;
  private generation = 0;

  register(image: MeasuredImage): () => void {
    this.images.add(image);
    this.refresh();
    return () => { this.images.delete(image); image.visible(false); };
  }

  setBounds(bounds: ImageRect): void { this.bounds = bounds; this.refresh(); }
  setActive(active: boolean): void { this.active = active; this.refresh(); }

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

/** One component-owned read. Offscreen changes revoke reads between chunks and late results. */
export class VisibleConversationImage<TImage> {
  private state: VisibleImageState<TImage> = { kind: 'closed' };
  private visible = false;
  private generation = 0;
  private disposed = false;

  constructor(private load: (current: () => boolean) => Promise<TImage>, private changed: (state: VisibleImageState<TImage>) => void) {}

  setVisible(visible: boolean): void {
    if (this.disposed) return;
    this.visible = visible;
    if (!visible && this.state.kind === 'loading') {
      this.generation++;
      this.publish({ kind: 'closed' });
    }
    if (visible && this.state.kind === 'closed') void this.read();
  }

  retry(): void { if (this.visible && this.state.kind === 'failed') void this.read(); }
  fail(message: string): void { if (!this.disposed) this.publish({ kind: 'failed', message }); }
  dispose(): void { this.disposed = true; this.generation++; this.state = { kind: 'closed' }; }

  private publish(state: VisibleImageState<TImage>): void { this.state = state; this.changed(state); }

  private async read(): Promise<void> {
    const generation = ++this.generation;
    const current = () => !this.disposed && this.visible && this.generation === generation;
    this.publish({ kind: 'loading' });
    try {
      const image = await this.load(current);
      if (current()) this.publish({ kind: 'image', image });
    } catch (error) {
      if (current()) this.publish({ kind: 'failed', message: error instanceof Error ? error.message : String(error) });
    }
  }
}
