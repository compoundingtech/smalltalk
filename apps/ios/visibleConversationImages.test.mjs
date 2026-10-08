import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ConversationImageViewport, VisibleConversationImage } from './visibleConversationImages.ts';
import { contentImageUri, loadConversationContent } from '../../clients/typescript/st3-views/conversationContent.ts';

const tick = async () => { await Promise.resolve(); await Promise.resolve(); await Promise.resolve(); };
const png = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 0]);
const reference = { ref: 'synthetic-visible-image', media_type: 'application/octet-stream', reason: 'on-demand' };
const chunk = (offset, end, ref = reference.ref) => ({ kind: 'conversation-content-chunk', ref, media_type: 'application/octet-stream', offset, size: png.length, data: png.subarray(offset, end).toString('base64'), ...(end < png.length ? { next_offset: end } : {}) });

test('visible native image loads real sequential chunks without a tap; overscan stays unfetched', async () => {
  const viewport = new ConversationImageViewport();
  viewport.setBounds({ x: 0, y: 100, width: 320, height: 400 });
  viewport.setActive(true);
  const reads = [];
  const states = [];
  const completed = Promise.withResolvers();
  const image = new VisibleConversationImage(viewport, async current => {
    const content = await loadConversationContent(reference, async (ref, offset) => {
      assert.equal(current(), true);
      reads.push([ref, offset]);
      return chunk(offset, offset === 0 ? 8 : png.length);
    });
    return contentImageUri(content);
  }, state => { states.push(state); if (state.kind === 'image') completed.resolve(); if (state.kind === 'failed') completed.reject(new Error(state.message)); });
  let rect = { x: 12, y: 510, width: 200, height: 150 };
  const unregister = viewport.register({ measure: done => done(rect), visible: shown => image.setVisible(shown) });
  await tick();
  assert.deepEqual(reads, [], 'mounted below the viewport is not visible');
  rect = { ...rect, y: 450 };
  viewport.refresh();
  await completed.promise;
  assert.deepEqual(reads, [[reference.ref, 0], [reference.ref, 8]]);
  assert.equal(states.at(-1).kind, 'image');
  assert.equal(states.at(-1).image, `data:image/png;base64,${png.toString('base64')}`);
  viewport.refresh();
  assert.equal(reads.length, 2, 'redrawing does not reload bytes');
  image.dispose();
  unregister();
});

test('scrolling offscreen revokes remaining chunks and ignores the late image', async () => {
  let resolveFirst;
  const first = new Promise(resolve => { resolveFirst = resolve; });
  const reads = [];
  const states = [];
  const image = new VisibleConversationImage(new ConversationImageViewport(), async current => {
    const content = await loadConversationContent(reference, async (ref, offset) => {
      if (!current()) throw new Error('Image is no longer visible.');
      reads.push(offset);
      return offset === 0 ? first : chunk(offset, png.length);
    });
    return contentImageUri(content);
  }, state => states.push(state));
  image.setVisible(true);
  image.setVisible(false);
  resolveFirst(chunk(0, 8));
  await tick();
  assert.deepEqual(reads, [0], 'no second offscreen chunk');
  assert.equal(states.at(-1).kind, 'closed');
  image.dispose();
});

test('unmount or revision replacement revokes the read and ignores late success', async () => {
  let finish;
  const states = [];
  const image = new VisibleConversationImage(new ConversationImageViewport(), () => new Promise(resolve => { finish = resolve; }), state => states.push(state));
  image.setVisible(true);
  image.dispose();
  finish('synthetic image');
  await tick();
  assert.deepEqual(states.map(state => state.kind), ['loading']);
});

test('automatic image failures wait for a visible explicit retry', async () => {
  let calls = 0;
  const states = [];
  const image = new VisibleConversationImage(new ConversationImageViewport(), async () => {
    if (++calls === 1) throw new Error('Synthetic owner is offline.');
    return 'synthetic retry image';
  }, state => states.push(state));
  image.setVisible(true);
  await tick();
  assert.equal(states.at(-1).kind, 'failed');
  image.setVisible(false);
  image.setVisible(true);
  await tick();
  assert.equal(calls, 1, 'visibility changes do not loop retries');
  image.setVisible(false);
  image.retry();
  assert.equal(calls, 1, 'retry cannot authorize an offscreen fetch');
  image.setVisible(true);
  image.retry();
  await tick();
  assert.equal(states.at(-1).kind, 'image');
  assert.equal(calls, 2);
  image.dispose();
});

test('inactive conversations and stale native measurements never authorize image reads', () => {
  const viewport = new ConversationImageViewport();
  viewport.setBounds({ x: 0, y: 0, width: 300, height: 500 });
  let measured;
  const visibility = [];
  const unregister = viewport.register({ measure: done => { measured = done; }, visible: shown => visibility.push(shown) });
  assert.deepEqual(visibility, [false]);
  viewport.setActive(true);
  const stale = measured;
  viewport.setActive(false);
  stale({ x: 0, y: 0, width: 200, height: 150 });
  assert.deepEqual(visibility, [false, false]);
  unregister();
});

test('all visible images share one owner read slot until each image completes every chunk', async () => {
  const viewport = new ConversationImageViewport();
  viewport.setBounds({ x: 0, y: 0, width: 320, height: 500 });
  viewport.setActive(true);
  const gates = Array.from({ length: 8 }, () => ({
    started: [Promise.withResolvers(), Promise.withResolvers()],
    replies: [Promise.withResolvers(), Promise.withResolvers()],
    completed: Promise.withResolvers(),
  }));
  const reads = [];
  let active = 0;
  let maxActive = 0;
  const images = gates.map((gate, index) => {
    const imageReference = { ...reference, ref: `synthetic-viewport-image-${index}` };
    const image = new VisibleConversationImage(viewport, async current => {
      const content = await loadConversationContent(imageReference, async (ref, offset) => {
        assert.equal(current(), true);
        const part = offset === 0 ? 0 : 1;
        reads.push([index, offset]);
        maxActive = Math.max(maxActive, ++active);
        gate.started[part].resolve();
        try { return await gate.replies[part].promise; }
        finally { active--; }
      });
      return contentImageUri(content);
    }, state => {
      if (state.kind === 'image') gate.completed.resolve(state.image);
      if (state.kind === 'failed') gate.completed.reject(new Error(state.message));
    });
    const unregister = viewport.register({
      measure: done => done({ x: 0, y: index * 40, width: 200, height: 40 }),
      visible: shown => image.setVisible(shown),
    });
    return { image, unregister, imageReference };
  });
  assert.deepEqual(reads, [[0, 0]], 'the entire viewport does not fan out owner requests');
  for (const [index, gate] of gates.entries()) {
    await gate.started[0].promise;
    assert.equal(active, 1);
    gate.replies[0].resolve(chunk(0, 8, images[index].imageReference.ref));
    await gate.started[1].promise;
    assert.equal(active, 1);
    assert.equal(reads.length, index * 2 + 2, 'the next image waits for the final chunk');
    gate.replies[1].resolve(chunk(8, png.length, images[index].imageReference.ref));
    assert.equal(await gate.completed.promise, `data:image/png;base64,${png.toString('base64')}`);
  }
  assert.equal(maxActive, 1);
  assert.equal(active, 0);
  assert.deepEqual(reads, gates.flatMap((_, index) => [[index, 0], [index, 8]]));
  for (const { image, unregister } of images) { image.dispose(); unregister(); }
});

test('offscreen reentry reuses the pending owner request without duplicate reads', async () => {
  const viewport = new ConversationImageViewport();
  const first = Promise.withResolvers();
  const completed = Promise.withResolvers();
  const reads = [];
  const states = [];
  const image = new VisibleConversationImage(viewport, async current => {
    const content = await loadConversationContent(reference, async (ref, offset) => {
      if (!current()) throw new Error('Image is no longer visible.');
      reads.push(offset);
      return offset === 0 ? first.promise : chunk(offset, png.length);
    });
    return contentImageUri(content);
  }, state => {
    states.push(state);
    if (state.kind === 'image') completed.resolve();
    if (state.kind === 'failed') completed.reject(new Error(state.message));
  });
  image.setVisible(true);
  for (let pass = 0; pass < 3; pass++) { image.setVisible(false); image.setVisible(true); }
  await tick();
  assert.deepEqual(reads, [0], 'reentry must not reacquire the pending owner request');
  assert.equal(states.at(-1).kind, 'loading');
  first.resolve(chunk(0, 8));
  await completed.promise;
  assert.deepEqual(reads, [0, 8]);
  assert.equal(states.at(-1).kind, 'image');
  image.dispose();
});

test('invisible and disposed queued images never fetch, and disposal waits for the active owner read', async () => {
  const viewport = new ConversationImageViewport();
  const blocked = Promise.withResolvers();
  const completed = Promise.withResolvers();
  const hiddenCompleted = Promise.withResolvers();
  const reads = [];
  const blockedStates = [];
  const first = new VisibleConversationImage(viewport, () => { reads.push('blocked'); return blocked.promise; }, state => blockedStates.push(state));
  const hidden = new VisibleConversationImage(viewport, async () => { reads.push('hidden'); return 'synthetic hidden image'; }, state => {
    if (state.kind === 'image') hiddenCompleted.resolve();
  });
  const disposed = new VisibleConversationImage(viewport, async () => { reads.push('disposed'); return 'synthetic disposed image'; }, () => {});
  const last = new VisibleConversationImage(viewport, async () => { reads.push('visible'); return 'synthetic visible image'; }, state => {
    if (state.kind === 'image') completed.resolve();
  });
  for (const image of [first, hidden, disposed, last]) image.setVisible(true);
  hidden.setVisible(false);
  disposed.dispose();
  first.dispose();
  disposed.setVisible(true);
  disposed.retry();
  await tick();
  assert.deepEqual(reads, ['blocked'], 'disposal cannot release a non-abortable owner request early');
  blocked.resolve('synthetic late image');
  await completed.promise;
  assert.deepEqual(reads, ['blocked', 'visible']);
  assert.deepEqual(blockedStates.map(state => state.kind), ['loading'], 'disposed results cannot publish');
  hidden.setVisible(true);
  await hiddenCompleted.promise;
  assert.deepEqual(reads, ['blocked', 'visible', 'hidden'], 'a canceled queue entry can reenter once');
  hidden.dispose();
  last.dispose();
});

test('offscreen chunk cancellation keeps the viewport queue occupied until the request settles', async () => {
  const viewport = new ConversationImageViewport();
  const firstChunk = Promise.withResolvers();
  const completed = Promise.withResolvers();
  const reads = [];
  const states = [];
  const first = new VisibleConversationImage(viewport, async current => {
    const content = await loadConversationContent(reference, async (ref, offset) => {
      if (!current()) throw new Error('Image is no longer visible.');
      reads.push(['first', offset]);
      return offset === 0 ? firstChunk.promise : chunk(offset, png.length);
    });
    return contentImageUri(content);
  }, state => states.push(state));
  const next = new VisibleConversationImage(viewport, async () => { reads.push(['next', 0]); return 'synthetic next image'; }, state => {
    if (state.kind === 'image') completed.resolve();
  });
  first.setVisible(true);
  next.setVisible(true);
  first.setVisible(false);
  await tick();
  assert.deepEqual(reads, [['first', 0]], 'the next image must not overlap an offscreen owner request');
  firstChunk.resolve(chunk(0, 8));
  await completed.promise;
  assert.deepEqual(reads, [['first', 0], ['next', 0]], 'the canceled image never fetches its next chunk');
  assert.equal(states.at(-1).kind, 'closed');
  first.dispose();
  next.dispose();
});

test('an owner failure while offscreen remains an explicit-retry failure and releases the queue', async () => {
  const viewport = new ConversationImageViewport();
  const owner = Promise.withResolvers();
  const failed = Promise.withResolvers();
  const retried = Promise.withResolvers();
  const nextCompleted = Promise.withResolvers();
  const reads = [];
  const image = new VisibleConversationImage(viewport, async () => {
    reads.push('image');
    return reads.filter(read => read === 'image').length === 1 ? owner.promise : 'synthetic retry image';
  }, state => {
    if (state.kind === 'failed') failed.resolve(state.message);
    if (state.kind === 'image') retried.resolve();
  });
  const next = new VisibleConversationImage(viewport, async () => { reads.push('next'); return 'synthetic next image'; }, state => {
    if (state.kind === 'image') nextCompleted.resolve();
  });
  image.setVisible(true);
  next.setVisible(true);
  image.setVisible(false);
  owner.reject(new Error('Synthetic owner is offline.'));
  assert.equal(await failed.promise, 'Synthetic owner is offline.');
  await nextCompleted.promise;
  image.setVisible(true);
  await tick();
  assert.deepEqual(reads, ['image', 'next'], 'reentry must not retry a genuine owner failure');
  image.setVisible(false);
  image.retry();
  assert.deepEqual(reads, ['image', 'next'], 'offscreen retry cannot enqueue a read');
  image.setVisible(true);
  image.retry();
  await retried.promise;
  assert.deepEqual(reads, ['image', 'next', 'image']);
  image.dispose();
  next.dispose();
});

test('reentry after a chunk revoked visibility waits for cancellation and cannot revive that read', async () => {
  const viewport = new ConversationImageViewport();
  const firstChunk = Promise.withResolvers();
  const completed = Promise.withResolvers();
  const reads = [];
  let calls = 0;
  const image = new VisibleConversationImage(viewport, async current => {
    const attempt = ++calls;
    reads.push(`image:${attempt}`);
    if (attempt === 1) {
      await firstChunk.promise;
      assert.equal(current(), false);
      image.setVisible(true);
      assert.equal(current(), false, 'an observed cancellation stays revoked after reentry');
      assert.equal(calls, 1, 'reentry waits for this canceled load to finish');
      throw new Error('Image is no longer visible.');
    }
    assert.equal(current(), true);
    return 'synthetic reentered image';
  }, state => {
    if (state.kind === 'image') completed.resolve();
    if (state.kind === 'failed') completed.reject(new Error(state.message));
  });
  const next = new VisibleConversationImage(viewport, async () => { reads.push('next'); return 'synthetic next image'; }, () => {});
  image.setVisible(true);
  next.setVisible(true);
  image.setVisible(false);
  firstChunk.resolve();
  await completed.promise;
  assert.deepEqual(reads, ['image:1', 'next', 'image:2']);
  image.dispose();
  next.dispose();
});
