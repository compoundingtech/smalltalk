import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { build, nodeKey, canPlace, planMove, rollup } from '@smalltalk/st3-views/sidebarView';
import { build as publicBuild } from '@smalltalk/st3-views';
import { project } from '../../../../apps/fractal-web/src/folders/core.mts';
import * as Schema from '@smalltalk/st3-client/schema';

const fixtures = JSON.parse(readFileSync(new URL('../../../../fixtures/clients/sidebar-v2.json', import.meta.url), 'utf8'));
const folder = id => ({ _tag: 'Folder', id });
const subject = subject => ({ _tag: 'Subject', subject });
const root = { _tag: 'Root' };
const built = (arrangement, inventory = []) => {
  const result = build(arrangement, inventory);
  assert.equal(result._tag, 'Built');
  return result.tree;
};
const empty = () => structuredClone(fixtures.cases.find(value => value.name === 'empty').arrangement);
const addFolder = (arrangement, id, parent = null, key = 'a0') => {
  arrangement.body.folders[id] = {
    name: { value: id, revision: 'claim/name' },
    position: { value: { parent, key }, revision: 'claim/position' },
    tombstone: null,
  };
  arrangement.resolved.parents[id] = parent;
};

assert.equal(fixtures.scope, 'folder-only-v1');
for (const fixture of fixtures.cases) {
  test(`shared sidebar strict Arrangement schema: ${fixture.name}`, () => {
    assert.doesNotThrow(() => Schema.decodeUnknownSync(Schema.Arrangement, 'strict')(fixture.arrangement));
  });
  test(`shared sidebar: ${fixture.name}`, () => {
    const tree = built(fixture.arrangement, fixture.inventory);
    assert.deepEqual({ ...tree, nodes: Object.fromEntries(tree.nodes) }, fixture.expected);
    // Fixed synthetic stamps are fixture adapters, never production revision chronology.
    const at = [1, 0, 'writer'];
    const doc = {
      folders: Object.fromEntries(Object.entries(fixture.arrangement.body.folders).map(([id, value]) => [id, {
        name: { value: value.name.value, at },
        position: { ...value.position.value, at },
        ...(value.tombstone === null ? {} : { deleted: at }),
      }])),
      placements: Object.fromEntries(Object.entries(fixture.arrangement.body.placements).map(([id, value]) => [id, { ...value.value, at }])),
    };
    const subjects = new Set([...fixture.inventory, ...Object.keys(doc.placements)]);
    const current = project(doc, subjects, { rootOrder: 'placement' });
    const currentFolder = value => ({ id: value.id, folders: value.folders.map(currentFolder), members: value.members });
    const seamFolder = node => {
      const children = tree.nodes.get(nodeKey(node)).children;
      return {
        id: node.id,
        folders: children.filter(child => child._tag === 'Folder').map(seamFolder),
        members: children.filter(child => child._tag === 'Subject').map(child => child.subject),
      };
    };
    assert.deepEqual({
      folders: tree.roots.filter(node => node._tag === 'Folder').map(seamFolder),
      unfiled: tree.roots.filter(node => node._tag === 'Subject').map(node => node.subject),
    }, { folders: current.folders.map(currentFolder), unfiled: current.unfiled });
    for (const move of fixture.moves) {
      assert.deepEqual(planMove(tree, move.intent), move.result);
      assert.deepEqual(canPlace(tree, move.intent), move.result._tag === 'Planned' ? { _tag: 'Allowed' } : move.result);
    }
    assert.deepEqual(Object.fromEntries(rollup(tree, new Set(fixture.needsYou))), fixture.rollup);
  });
}

test('public entry exports the React-free projection and keys are disjoint', () => {
  assert.equal(publicBuild, build);
  assert.equal(nodeKey(folder('subject:x')), 'folder:subject:x');
  assert.equal(nodeKey(subject('folder:x')), 'subject:folder:x');
  assert.notEqual(nodeKey(folder('x')), nodeKey(subject('x')));
});

test('build requires live arrangement, supported version, and complete authoritative resolution', () => {
  const arrangement = empty();
  arrangement.deleted = true;
  assert.deepEqual(build(arrangement, []), { _tag: 'Refused', error: { _tag: 'ArrangementUnavailable' } });
  arrangement.deleted = false;
  arrangement.body.version = 2;
  assert.deepEqual(build(arrangement, []), { _tag: 'Refused', error: { _tag: 'Unsupported', required: 1, advertised: 2 } });
  arrangement.body.version = 1;
  const resolved = arrangement.resolved;
  delete arrangement.resolved;
  assert.deepEqual(build(arrangement, []), { _tag: 'Refused', error: { _tag: 'NotReady' } });
  arrangement.resolved = resolved;
  addFolder(arrangement, 'a');
  delete arrangement.resolved.parents.a;
  assert.deepEqual(build(arrangement, []), { _tag: 'Refused', error: { _tag: 'NotReady' } });
  arrangement.resolved.parents.a = null;
  arrangement.body.placements['agent/dormant'] = { value: { folder: null, key: 'a0' }, revision: 'claim/place' };
  assert.deepEqual(build(arrangement, []), { _tag: 'Refused', error: { _tag: 'NotReady' } });
});

test('build refuses nonlive resolved parents and cycles rather than guessing from revisions', () => {
  const arrangement = empty();
  addFolder(arrangement, 'a');
  addFolder(arrangement, 'b', 'a');
  arrangement.resolved.parents.a = 'unknown';
  assert.deepEqual(build(arrangement, []), { _tag: 'Refused', error: { _tag: 'InvalidResolution' } });
  arrangement.resolved.parents.a = 'b';
  assert.deepEqual(build(arrangement, []), { _tag: 'Refused', error: { _tag: 'InvalidResolution' } });
  arrangement.resolved.parents.a = 'a';
  assert.deepEqual(build(arrangement, []), { _tag: 'Refused', error: { _tag: 'InvalidResolution' } });
  arrangement.resolved.parents.a = null;
  arrangement.body.folders.a.tombstone = { value: true, revision: 'claim/delete' };
  assert.deepEqual(build(arrangement, []), { _tag: 'Refused', error: { _tag: 'InvalidResolution' } });
});

test('ordinal ordering uses Unicode scalars in separate placement domains and deduplicates inventory', () => {
  const arrangement = empty();
  addFolder(arrangement, 'z', null, 'z0');
  addFolder(arrangement, '\u{10000}', null, 'a0');
  addFolder(arrangement, '\ue000', null, 'a0');
  arrangement.body.placements['agent/placed'] = { value: { folder: null, key: 'a0' }, revision: 'claim/place' };
  arrangement.resolved.folders['agent/placed'] = null;
  const tree = built(arrangement, ['agent/\u{10000}', 'agent/\ue000', 'agent/placed', 'agent/\ue000']);
  assert.deepEqual(tree.roots, [folder('\ue000'), folder('\u{10000}'), folder('z'), subject('agent/placed'), subject('agent/\ue000'), subject('agent/\u{10000}')]);
  assert.equal(tree.nodes.size, 6);
});

test('move refusal precedence, structural identity, anchor domains and no-change semantics', () => {
  const arrangement = empty();
  addFolder(arrangement, 'a');
  addFolder(arrangement, 'b', 'a');
  addFolder(arrangement, 'gone');
  arrangement.body.folders.gone.tombstone = { value: true, revision: 'claim/delete' };
  const tree = built(arrangement, ['agent/a', 'agent/b']);
  const move = (node, parent, before = null) => planMove(tree, { node, parent, before });
  assert.deepEqual(move(subject('missing'), subject('agent/a')), { _tag: 'Refused', refusal: { _tag: 'NodeMissing', node: subject('missing') } });
  assert.deepEqual(move(subject('agent/a'), subject('agent/a')), { _tag: 'Refused', refusal: { _tag: 'ParentNotAllowed', parent: subject('agent/a'), cause: 'SubjectParentUnsupported' } });
  assert.deepEqual(move(folder('a'), folder('a')), { _tag: 'Refused', refusal: { _tag: 'Self', node: folder('a') } });
  for (const [id, cause] of [['gone', 'FolderDeleted'], ['missing', 'Unknown']]) {
    assert.deepEqual(move(subject('agent/a'), folder(id)), { _tag: 'Refused', refusal: { _tag: 'ParentMissing', parent: folder(id), cause } });
  }
  assert.deepEqual(move(folder('a'), folder('b')), {
    _tag: 'Refused', refusal: { _tag: 'Cycle', node: folder('a'), parent: folder('b'), path: [
      { node: folder('a'), source: 'person' }, { node: folder('b'), source: 'person' }, { node: folder('a'), source: 'person' },
    ] },
  });
  for (const before of [folder('a'), subject('agent/a')]) {
    assert.deepEqual(move(subject('agent/a'), root, before), { _tag: 'Refused', refusal: { _tag: 'AnchorMoved', before, parent: root, foundUnder: root } });
  }
  assert.deepEqual(move(subject('agent/a'), root, subject('missing')), { _tag: 'Refused', refusal: { _tag: 'AnchorMoved', before: subject('missing'), parent: root } });
  assert.deepEqual(move(subject('agent/a'), root, subject('agent/b')), { _tag: 'Refused', refusal: { _tag: 'NoChange', node: subject('agent/a'), parent: root } });
  assert.equal(move(subject('agent/a'), root)._tag, 'Planned');
  assert.deepEqual(canPlace(tree, { node: subject('agent/a'), parent: folder('b'), before: null }), { _tag: 'Allowed' });
  assert.deepEqual(canPlace(tree, { node: folder('a'), parent: folder('b'), before: null }), move(folder('a'), folder('b')));
});

test('rollup includes empty folders and counts nested dormant placements once', () => {
  const arrangement = empty();
  addFolder(arrangement, 'a');
  addFolder(arrangement, 'b', 'a');
  addFolder(arrangement, 'empty');
  arrangement.body.placements['session/dormant'] = { value: { folder: 'b', key: 'a0' }, revision: 'claim/place' };
  arrangement.resolved.folders['session/dormant'] = 'b';
  const tree = built(arrangement, ['session/dormant', 'session/dormant', 'agent/root']);
  assert.deepEqual(Object.fromEntries(rollup(tree, new Set(['session/dormant', 'agent/root', 'agent/absent']))), {
    'folder:a': { needsYou: 1, display: 'Always' },
    'folder:b': { needsYou: 1, display: 'Always' },
    'folder:empty': { needsYou: 0, display: 'Always' },
  });
});
