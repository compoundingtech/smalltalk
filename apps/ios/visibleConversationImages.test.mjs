import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ConversationImageViewport, VisibleConversationImage } from './visibleConversationImages.ts';
import { contentImageUri, loadConversationContent } from '../../clients/typescript/st3-views/conversationContent.ts';

const tick = async () => { await Promise.resolve(); await Promise.resolve(); await Promise.resolve(); };
const png = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 0]);
const reference = { ref: 'synthetic-visible-image', media_type: 'application/octet-stream', reason: 'on-demand' };
const chunk = (offset, end) => ({ kind: 'conversation-content-chunk', ref: reference.ref, media_type: 'application/octet-stream', offset, size: png.length, data: png.subarray(offset, end).toString('base64'), ...(end < png.length ? { next_offset: end } : {}) });

test('visible native image loads real sequential chunks without a tap; overscan stays unfetched', async () => {
  const viewport = new ConversationImageViewport();
  viewport.setBounds({ x: 0, y: 100, width: 320, height: 400 });
  viewport.setActive(true);
  const reads = [];
  const states = [];
  const completed = Promise.withResolvers();
  const image = new VisibleConversationImage(async current => {
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
  const image = new VisibleConversationImage(async current => {
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

test('unmount or revision replacement releases the request and ignores late success', async () => {
  let finish;
  const states = [];
  const image = new VisibleConversationImage(() => new Promise(resolve => { finish = resolve; }), state => states.push(state));
  image.setVisible(true);
  image.dispose();
  finish('synthetic image');
  await tick();
  assert.deepEqual(states.map(state => state.kind), ['loading']);
});

test('automatic image failures wait for a visible explicit retry', async () => {
  let calls = 0;
  const states = [];
  const image = new VisibleConversationImage(async () => {
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
