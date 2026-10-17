// What an image in a conversation, a message or an ask says before and instead of its pixels, as
// stui says it (docs/clients/ui-contract.md, "Images"): its name, type and real size, that it is
// loading, or why it cannot be shown. Never the file's path. Kept apart from React so it is tested.
import { megabytes } from './images';
import type { VisibleImageState } from './visibleConversationImages';

export type ImageFacts = { name?: string; mediaType: string; size?: number };

/** `garden.png · image/png · 71 KB`; a size st does not know yet says so. */
export function imageLabel(image: ImageFacts): string {
  const size = image.size === undefined || image.size <= 0 ? 'size unknown' : megabytes(image.size);
  return [image.name, image.mediaType || 'image', size].filter(Boolean).join(' · ');
}

/** Why an image cannot be shown, in words, and whether reading it again may help. */
export function imageFailure(code: string | undefined, message: string): { words: string; retry: boolean } {
  switch (code) {
    // The owner of the message keeps the bytes; it is off or out of reach (docs/st3/attachments.md).
    case 'remote-unavailable': return { words: `${message.replace(/\.$/, '')}. The computer that has this image cannot be reached right now (remote unavailable).`, retry: true };
    case 'blob-expired': return { words: 'This image has expired: st no longer keeps it.', retry: false };
    case 'forbidden': return { words: 'You cannot read this image.', retry: false };
    default: return { words: message || 'The image could not be read.', retry: true };
  }
}

/** The line under (or instead of) an image for each state: never blank, never a spinner alone. */
export function imageWords<T>(state: VisibleImageState<T>, image: ImageFacts): { text: string; tone: 'dim' | 'fault'; retry: boolean } {
  const label = imageLabel(image);
  switch (state.kind) {
    case 'loading': return { text: `Loading image… · ${label}`, tone: 'dim', retry: false };
    case 'failed': {
      const failure = imageFailure(state.code, state.message);
      return { text: `${label}\n${failure.words}`, tone: 'fault', retry: failure.retry };
    }
    default: return { text: label, tone: 'dim', retry: false };
  }
}

/**
 * Images read from st, as data URIs, kept up to a total size: the newest stay, the oldest leave.
 * A URI is about 4/3 of its bytes. The decoded pixels are not here: React Native's iOS loader
 * decodes an image at the size of the view that draws it, which is how a huge image is scaled
 * down before it is held, and frees them when that view goes.
 */
export class ImageCache {
  private kept = new Map<string, string>();
  private total = 0;
  constructor(private readonly limit: number) { if (limit < 1) throw new Error('cache limit must be positive'); }

  get(key: string): string | undefined {
    const uri = this.kept.get(key);
    if (uri !== undefined) { this.kept.delete(key); this.kept.set(key, uri); }
    return uri;
  }

  set(key: string, uri: string): void {
    this.delete(key);
    // One image larger than the whole budget is shown but not kept.
    if (uri.length > this.limit) return;
    this.kept.set(key, uri);
    this.total += uri.length;
    for (const [oldest, kept] of this.kept) {
      if (this.total <= this.limit) break;
      this.kept.delete(oldest);
      this.total -= kept.length;
    }
  }

  delete(key: string): void {
    const uri = this.kept.get(key);
    if (uri === undefined) return;
    this.kept.delete(key);
    this.total -= uri.length;
  }

  get size(): number { return this.total; }
  get count(): number { return this.kept.size; }
}

/** One read of an image at a time, then its URI from the cache until it leaves it. */
export class ImageReads {
  private reading = new Map<string, Promise<string>>();
  constructor(readonly cache: ImageCache) {}

  read(key: string, load: () => Promise<string>): Promise<string> {
    const kept = this.cache.get(key);
    if (kept !== undefined) return Promise.resolve(kept);
    const pending = this.reading.get(key);
    if (pending) return pending;
    // A failed read is not kept: the next time the image is shown it is read again.
    const reading = load().then(uri => { this.cache.set(key, uri); return uri; });
    this.reading.set(key, reading);
    const settled = () => { if (this.reading.get(key) === reading) this.reading.delete(key); };
    reading.then(settled, settled);
    return reading;
  }
}

/**
 * The cache key of an image: the message (or ask step) it is read by, then its sha256. st decides
 * per carrier whether the session may read a sha256, so bytes read through one carrier are never
 * handed to another that names the same sha256, such as an attachment element typed into a prompt.
 */
export function imageKey(image: { sha256: string; message: string }): string { return `${image.message} ${image.sha256}`; }

/** The phone's images read from st: up to 16 MB of them stay for when they are shown again. */
export const IMAGES = new ImageReads(new ImageCache(16 * 1024 * 1024));
