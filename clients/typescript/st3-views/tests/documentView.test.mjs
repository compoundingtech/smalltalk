import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  backOnTrail, documentLinks, documentListPlaceholder, documentUnavailable, documentView, emptyDocumentList,
  explainUnavailable, failDocumentList, openOnTrail, parseDocumentReference, pushDocumentPage,
} from '../documentView.ts';

const fixture = JSON.parse(readFileSync(new URL('../../../../fixtures/clients/documents-v1.json', import.meta.url), 'utf8'));
const bytes = text => [...new TextEncoder().encode(text)];

test('finds the links the fixture pins', () => {
  for (const item of fixture.links) assert.deepEqual(documentLinks(item.text), item.links, item.name);
});

test('a long document renders unchanged', () => {
  const text = fixture.links[0].text;
  const reference = `doc/garden/plan@${'c'.repeat(64)}`;
  const view = documentView('doc/garden/plan', {
    reference, bytes: bytes(text), name: 'doc/garden/plan', version: 'c'.repeat(64), size: text.length,
    changed_at_unix_ms: 1_700_000_000_000, latest: true, owner: 'agent/garden/writer',
  });
  assert.equal(view.state, 'ready');
  assert.equal(view.text, text);
  assert.ok(view.text.includes('| North | Ada |'));
  assert.ok(view.text.includes('```kdl\nstep "plant"'));
  assert.ok(view.text.includes('     1. Water the beds'));
  assert.equal(view.shortVersion, 'cccccccccccc');
  assert.equal(view.changedAtUnixMs, 1_700_000_000_000);
  assert.equal(view.links.length, 9);
});

test('an older daemon answer still names the document', () => {
  const reference = `doc/garden/plan@${'d'.repeat(64)}`;
  const view = documentView(reference, { reference, bytes: bytes('# Plan') });
  assert.equal(view.state, 'ready');
  assert.equal(view.name, 'doc/garden/plan');
  assert.equal(view.version, 'd'.repeat(64));
  assert.equal(view.size, 6);
  assert.equal(view.changedAtUnixMs, null);
});

test('bytes that are not text say so', () => {
  const view = documentView('doc/garden/photo', { reference: `doc/garden/photo@${'e'.repeat(64)}`, bytes: [0xff, 0xfe, 0] });
  assert.equal(view.state, 'unavailable');
  assert.equal(view.why, 'not-text');
  assert.match(view.explanation, /doc\/garden\/photo/);
});

test('errors map to the states the fixture pins', () => {
  for (const item of fixture.errors) {
    const why = documentUnavailable(item.status, item.code);
    assert.equal(why, item.state, JSON.stringify(item));
    assert.ok(explainUnavailable(why, 'doc/x').length > 0);
  }
});

test('following a link cycle returns to the earlier page', () => {
  let trail = [];
  for (const reference of fixture.trail.open) trail = openOnTrail(trail, reference);
  assert.deepEqual(trail, fixture.trail.expected);
  trail = backOnTrail(trail);
  assert.deepEqual(trail, ['doc/cycle/a']);
  assert.deepEqual(backOnTrail(trail), ['doc/cycle/a']);
});

const item = (name, version) => ({
  reference: `${name}@${version.repeat(64)}`, name, version: version.repeat(64), size: 10,
  changed_at_unix_ms: 1_700_000_000_000, latest: true,
});

test('the list pages and keeps its three states apart', () => {
  let list = emptyDocumentList('doc/garden/');
  assert.equal(documentListPlaceholder(list), 'Loading…');
  list = failDocumentList(list, 'remote-unavailable');
  assert.match(documentListPlaceholder(list), /did not answer/);
  list = pushDocumentPage(list, { items: [item('doc/garden/a', 'a'), item('doc/garden/b', 'b')], has_more: true, limit: 2, history: false, next_cursor: 'document/next' });
  assert.equal(list.nextCursor, 'document/next');
  list = pushDocumentPage(list, { items: [item('doc/garden/b', 'b'), item('doc/garden/c', 'c')], has_more: false, limit: 2, history: false, next_cursor: null });
  assert.deepEqual(list.rows.map(row => row.name), ['doc/garden/a', 'doc/garden/b', 'doc/garden/c']);
  assert.equal(list.nextCursor, null);
  assert.equal(list.rows[0].shortVersion, 'aaaaaaaaaaaa');
  const empty = pushDocumentPage(emptyDocumentList('doc/none/'), { items: [], has_more: false, limit: 50, history: false });
  assert.equal(documentListPlaceholder(empty), 'No documents under doc/none/.');
});

test('references parse only document names', () => {
  const hash = 'f'.repeat(64);
  assert.deepEqual(parseDocumentReference(`doc/a@${hash}`), { name: 'doc/a', version: hash });
  assert.deepEqual(parseDocumentReference('doc/a'), { name: 'doc/a', version: null });
  for (const invalid of ['doc/', 'doc/a/', 'doc/../a', 'doc/a@short', 'message/a', 'docs/a']) assert.equal(parseDocumentReference(invalid), null, invalid);
});
