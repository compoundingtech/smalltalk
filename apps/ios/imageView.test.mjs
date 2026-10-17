import assert from 'node:assert/strict';
import { test } from 'node:test';
import { deflateSync } from 'node:zlib';
import { ImageCache, ImageReads, imageFailure, imageKey, imageLabel, imageWords } from './imageView.ts';
import { deliveredImages } from '@smalltalk/st3-views';
import { ConversationImageViewport, VisibleConversationImage, errorCode } from './visibleConversationImages.ts';
import { encodeBase64 } from './images.ts';

// A PNG drawn here, never a real picture: a width×height gradient, as the audit's draw.py draws.
function drawnPng(width, height) {
  const crc = buffer => {
    let c = ~0;
    for (const byte of buffer) { c ^= byte; for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1)); }
    return ~c >>> 0;
  };
  const chunk = (type, data) => {
    const body = Buffer.concat([Buffer.from(type), data]);
    const out = Buffer.alloc(12 + data.length);
    out.writeUInt32BE(data.length, 0); body.copy(out, 4); out.writeUInt32BE(crc(body), 8 + data.length);
    return out;
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0); header.writeUInt32BE(height, 4); header[8] = 8; header[9] = 2;
  const rows = Buffer.alloc((width * 3 + 1) * height);
  for (let y = 0; y < height; y++) for (let x = 0; x < width; x++) {
    const at = y * (width * 3 + 1) + 1 + x * 3;
    rows[at] = (x * 255 / width) | 0; rows[at + 1] = (y * 255 / height) | 0; rows[at + 2] = 120;
  }
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', header), chunk('IDAT', deflateSync(rows)), chunk('IEND', Buffer.alloc(0))]);
}

const lawn = drawnPng(64, 40);
const uri = `data:image/png;base64,${encodeBase64(lawn)}`;
const facts = { name: 'lawn.png', mediaType: 'image/png', size: 71968 };
const ownerError = (code, message) => Object.assign(new Error(message), { response: { code, message } });
const tick = async () => { for (let i = 0; i < 4; i++) await Promise.resolve(); };

test('the placeholder names the type and real size, never a path; an unknown size says so', () => {
  assert.equal(imageLabel(facts), 'lawn.png · image/png · 70 KB');
  assert.equal(imageLabel({ mediaType: 'image/png', size: undefined }), 'image/png · size unknown');
  assert.equal(imageWords({ kind: 'closed' }, facts).text, 'lawn.png · image/png · 70 KB');
  assert.deepEqual(imageWords({ kind: 'loading' }, facts), { text: 'Loading image… · lawn.png · image/png · 70 KB', tone: 'dim', retry: false });
});

test('an owner out of reach and an expired image say so in different words; only one retries', () => {
  const away = imageWords({ kind: 'failed', code: 'remote-unavailable', message: 'pond cannot be reached right now' }, facts);
  assert.equal(away.tone, 'fault');
  assert.equal(away.retry, true);
  assert.match(away.text, /pond cannot be reached right now\. The computer that has this image cannot be reached right now \(remote unavailable\)\./);
  const gone = imageWords({ kind: 'failed', code: 'blob-expired', message: 'blob expired' }, facts);
  assert.equal(gone.retry, false);
  assert.match(gone.text, /has expired/);
  assert.equal(imageFailure(undefined, 'offline').words, 'offline');
  assert.equal(imageFailure(undefined, 'offline').retry, true);
  assert.equal(errorCode(ownerError('remote-unavailable', 'x')), 'remote-unavailable');
  assert.equal(errorCode(new Error('plain')), undefined);
});

test('a failed read carries its code; retry reads again; leaving the screen releases the bytes', async () => {
  const viewport = new ConversationImageViewport();
  viewport.setBounds({ x: 0, y: 0, width: 320, height: 600 });
  viewport.setActive(true);
  const states = [];
  let reads = 0;
  const image = new VisibleConversationImage(viewport, async () => {
    reads++;
    if (reads === 1) throw ownerError('remote-unavailable', 'pond cannot be reached right now');
    return uri;
  }, state => states.push(state));
  let rect = { x: 0, y: 100, width: 200, height: 150 };
  const unregister = viewport.register({ measure: done => done(rect), visible: shown => image.setVisible(shown) });
  await tick();
  assert.deepEqual(states.map(state => state.kind), ['loading', 'failed']);
  assert.equal(states.at(-1).code, 'remote-unavailable');
  assert.ok(imageWords(states.at(-1), facts).retry);
  image.retry();
  await tick();
  assert.equal(states.at(-1).kind, 'image');
  assert.equal(states.at(-1).image, uri);
  // Scrolled away: the image's URI is no longer held by its view.
  rect = { ...rect, y: 2000 };
  viewport.refresh();
  assert.equal(states.at(-1).kind, 'closed');
  rect = { ...rect, y: 100 };
  viewport.refresh();
  await tick();
  assert.equal(states.at(-1).kind, 'image', 'shown again when it comes back');
  image.dispose();
  unregister();
});

test('images read from st are kept up to a total size, oldest out first; one read per image at a time', async () => {
  const cache = new ImageCache(uri.length * 2 + 10);
  cache.set('a', uri); cache.set('b', uri);
  assert.equal(cache.count, 2);
  cache.get('a');
  cache.set('c', uri);
  assert.equal(cache.get('b'), undefined, 'the least recently shown leaves');
  assert.equal(cache.get('a'), uri);
  assert.ok(cache.size <= uri.length * 2 + 10);
  const huge = `data:image/png;base64,${'A'.repeat(uri.length * 3)}`;
  cache.set('huge', huge);
  assert.equal(cache.get('huge'), undefined, 'larger than the whole budget: shown, not kept');
  assert.equal(cache.count, 2);

  const reads = new ImageReads(new ImageCache(1 << 20));
  let loads = 0;
  const pending = Promise.withResolvers();
  const load = () => { loads++; return pending.promise; };
  const first = reads.read('lawn', load), second = reads.read('lawn', load);
  pending.resolve(uri);
  assert.equal(await first, uri);
  assert.equal(await second, uri);
  assert.equal(await reads.read('lawn', load), uri);
  assert.equal(loads, 1);
  let failing = 0;
  await assert.rejects(reads.read('away', () => { failing++; return Promise.reject(new Error('offline')); }));
  await assert.rejects(reads.read('away', () => { failing++; return Promise.reject(new Error('offline')); }));
  assert.equal(failing, 2, 'a failed read is tried again');
});

test('an attachment element typed into a prompt is only a box st answers for; it never reaches bytes another message read', async () => {
  // Anyone who can put text into a prompt can write an element naming any sha256, path, size and
  // message. The phone takes only the sha256 and asks st for it by that carrier, which refuses a
  // sha256 the message does not carry (client_blobs.rs: an_image_uploaded_by_one_person_reaches_a_message_and_only_its_readers).
  const sha = 'cd'.repeat(32);
  const typed = `look <attachment path="/srv/pond/private/${sha}.png" media_type="image/png" bytes="999999999" name="lawn.png"/>`;
  const { text, images } = deliveredImages(typed, 'message/forged');
  assert.equal(text, 'look');
  assert.deepEqual(images, [{ sha256: sha, message: 'message/forged', mediaType: 'image/png', name: 'lawn.png', size: 999999999 }]);

  // The same sha256 already read through the message that carries it stays with that message.
  const reads = new ImageReads(new ImageCache(1024 * 1024));
  assert.equal(await reads.read(imageKey({ sha256: sha, message: 'message/real' }), async () => uri), uri);
  const asked = [];
  const viewport = new ConversationImageViewport();
  viewport.setBounds({ x: 0, y: 0, width: 320, height: 600 });
  viewport.setActive(true);
  const states = [];
  const image = new VisibleConversationImage(viewport, () => reads.read(imageKey(images[0]), async () => {
    asked.push(images[0].message);
    throw ownerError('forbidden', 'this attachment is not in a conversation the session may read');
  }), state => states.push(state));
  const unregister = viewport.register({ measure: done => done({ x: 0, y: 0, width: 200, height: 150 }), visible: shown => image.setVisible(shown) });
  await tick();
  assert.deepEqual(asked, ['message/forged'], 'st is asked by the forged carrier, not answered from the cache');
  assert.deepEqual(states.map(state => state.kind), ['loading', 'failed']);
  assert.equal(states.at(-1).code, 'forbidden');
  const words = imageWords(states.at(-1), { name: images[0].name, mediaType: images[0].mediaType, size: images[0].size });
  assert.equal(words.text, 'lawn.png · image/png · 953.7 MB\nYou cannot read this image.');
  assert.equal(words.retry, false);
  assert.ok(!words.text.includes('/srv/pond') && !words.text.includes('private'), words.text);
  image.dispose();
  unregister();
});
