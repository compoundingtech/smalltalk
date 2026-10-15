import type { Arrangement } from '@smalltalk/st3-client';

export type NodeRef = { _tag: 'Folder'; id: string } | { _tag: 'Subject'; subject: string };
export type Parent = { _tag: 'Root' } | NodeRef;
export type NodeKey = `folder:${string}` | `subject:${string}`;
export type ProjectedNode = {
  node: NodeRef;
  parent: Parent;
  shownUnder: Parent;
  key?: string;
  revision?: string;
  source: 'person' | 'fallback';
  children: NodeRef[];
};
export type SidebarTree = {
  roots: NodeRef[];
  nodes: Map<NodeKey, ProjectedNode>;
  arrangementRevision: string;
  deletedFolders: string[];
};
export type ProjectionError =
  | { _tag: 'NotReady' }
  | { _tag: 'ArrangementUnavailable' }
  | { _tag: 'Unsupported'; required: 1; advertised: number }
  | { _tag: 'InvalidResolution' };
export type BuildResult = { _tag: 'Built'; tree: SidebarTree } | { _tag: 'Refused'; error: ProjectionError };
export type MoveIntent = { node: NodeRef; parent: Parent; before: NodeRef | null };
export type CycleStep = { node: NodeRef; source: 'person' | 'fallback' };
export type MoveRefusal =
  | { _tag: 'NodeMissing'; node: NodeRef }
  | { _tag: 'ParentNotAllowed'; parent: Parent; cause: 'SubjectParentUnsupported' }
  | { _tag: 'Self'; node: NodeRef }
  | { _tag: 'ParentMissing'; parent: Parent; cause: 'Unknown' | 'FolderDeleted' }
  | { _tag: 'Cycle'; node: NodeRef; parent: Parent; path: CycleStep[] }
  | { _tag: 'AnchorMoved'; before: NodeRef; parent: Parent; foundUnder?: Parent }
  | { _tag: 'NoChange'; node: NodeRef; parent: Parent };
export type MovePlan = { _tag: 'Planned'; intent: MoveIntent } | { _tag: 'Refused'; refusal: MoveRefusal };
export type PlacementCheck = { _tag: 'Allowed' } | { _tag: 'Refused'; refusal: MoveRefusal };
export type Rollup = { needsYou: number; display: 'Always' };

export const nodeKey = (node: NodeRef): NodeKey => node._tag === 'Folder' ? `folder:${node.id}` : `subject:${node.subject}`;
const folderParent = (id: string | null): Parent => id === null ? { _tag: 'Root' } : { _tag: 'Folder', id };
const sameParent = (left: Parent, right: Parent): boolean => left._tag === 'Root'
  ? right._tag === 'Root'
  : right._tag !== 'Root' && nodeKey(left) === nodeKey(right);

// Rust's string Ord compares UTF-8, not JavaScript's UTF-16 code units or locale order.
const ordinal = (left: string, right: string): number => {
  let a = 0;
  let b = 0;
  while (a < left.length && b < right.length) {
    const x = left.codePointAt(a)!;
    const y = right.codePointAt(b)!;
    if (x !== y) return x < y ? -1 : 1;
    a += x > 0xffff ? 2 : 1;
    b += y > 0xffff ? 2 : 1;
  }
  return a < left.length ? 1 : b < right.length ? -1 : 0;
};

/** Project authoritative folder resolution without interpreting inventory as lifecycle authority. */
export const build = (arrangement: Arrangement, inventory: readonly string[]): BuildResult => {
  if (arrangement.deleted) return { _tag: 'Refused', error: { _tag: 'ArrangementUnavailable' } };
  if (arrangement.body.version !== 1) {
    return { _tag: 'Refused', error: { _tag: 'Unsupported', required: 1, advertised: arrangement.body.version } };
  }
  const resolved = arrangement.resolved;
  if (!resolved) return { _tag: 'Refused', error: { _tag: 'NotReady' } };
  const tree: SidebarTree = { roots: [], nodes: new Map(), arrangementRevision: arrangement.revision, deletedFolders: [] };
  for (const [id, folder] of Object.entries(arrangement.body.folders)) {
    if (folder.tombstone !== null) {
      tree.deletedFolders.push(id);
      continue;
    }
    if (!Object.hasOwn(resolved.parents, id)) return { _tag: 'Refused', error: { _tag: 'NotReady' } };
    const node: NodeRef = { _tag: 'Folder', id };
    tree.nodes.set(nodeKey(node), {
      node, parent: folderParent(folder.position.value.parent), shownUnder: folderParent(resolved.parents[id]!),
      key: folder.position.value.key, revision: folder.position.revision, source: 'person', children: [],
    });
  }
  for (const [subject, placement] of Object.entries(arrangement.body.placements)) {
    if (!Object.hasOwn(resolved.folders, subject)) return { _tag: 'Refused', error: { _tag: 'NotReady' } };
    const node: NodeRef = { _tag: 'Subject', subject };
    tree.nodes.set(nodeKey(node), {
      node, parent: folderParent(placement.value.folder), shownUnder: folderParent(resolved.folders[subject]!),
      key: placement.value.key, revision: placement.revision, source: 'person', children: [],
    });
  }
  for (const subject of inventory) {
    const node: NodeRef = { _tag: 'Subject', subject };
    if (!tree.nodes.has(nodeKey(node))) {
      tree.nodes.set(nodeKey(node), { node, parent: { _tag: 'Root' }, shownUnder: { _tag: 'Root' }, source: 'fallback', children: [] });
    }
  }
  for (const entry of tree.nodes.values()) {
    if (entry.shownUnder._tag !== 'Root' && !tree.nodes.has(nodeKey(entry.shownUnder))) {
      return { _tag: 'Refused', error: { _tag: 'InvalidResolution' } };
    }
  }
  const checked = new Set<NodeKey>();
  for (const entry of tree.nodes.values()) {
    if (entry.node._tag !== 'Folder' || checked.has(nodeKey(entry.node))) continue;
    const path = new Set<NodeKey>();
    let current: ProjectedNode | undefined = entry;
    while (current && !checked.has(nodeKey(current.node))) {
      const key = nodeKey(current.node);
      if (path.has(key)) return { _tag: 'Refused', error: { _tag: 'InvalidResolution' } };
      path.add(key);
      current = current.shownUnder._tag === 'Root' ? undefined : tree.nodes.get(nodeKey(current.shownUnder));
    }
    for (const key of path) checked.add(key);
  }
  const ordered = [...tree.nodes.values()].sort((left, right) => {
    const domain = (entry: ProjectedNode): number => entry.node._tag === 'Folder' ? 0 : entry.key !== undefined ? 1 : 2;
    return domain(left) - domain(right) || ordinal(left.key ?? '', right.key ?? '')
      || ordinal(left.node._tag === 'Folder' ? left.node.id : left.node.subject, right.node._tag === 'Folder' ? right.node.id : right.node.subject);
  });
  for (const entry of ordered) {
    if (entry.shownUnder._tag === 'Root') tree.roots.push(entry.node);
    else tree.nodes.get(nodeKey(entry.shownUnder))!.children.push(entry.node);
  }
  tree.deletedFolders.sort(ordinal);
  return { _tag: 'Built', tree };
};

/** Validate a semantic move; ordering-key generation and writes belong to the caller. */
export const planMove = (tree: SidebarTree, intent: MoveIntent): MovePlan => {
  const { node, parent, before } = intent;
  const entry = tree.nodes.get(nodeKey(node));
  const refuse = (refusal: MoveRefusal): MovePlan => ({ _tag: 'Refused', refusal });
  if (!entry) return refuse({ _tag: 'NodeMissing', node });
  if (parent._tag === 'Subject') return refuse({ _tag: 'ParentNotAllowed', parent, cause: 'SubjectParentUnsupported' });
  if (sameParent(node, parent)) return refuse({ _tag: 'Self', node });
  if (parent._tag === 'Folder' && !tree.nodes.has(nodeKey(parent))) {
    return refuse({ _tag: 'ParentMissing', parent, cause: tree.deletedFolders.includes(parent.id) ? 'FolderDeleted' : 'Unknown' });
  }
  if (parent._tag === 'Folder') {
    const path: CycleStep[] = [{ node, source: entry.source }];
    let ancestor: ProjectedNode | undefined = tree.nodes.get(nodeKey(parent));
    while (ancestor) {
      path.push({ node: ancestor.node, source: ancestor.source });
      if (nodeKey(ancestor.node) === nodeKey(node)) return refuse({ _tag: 'Cycle', node, parent, path });
      ancestor = ancestor.shownUnder._tag === 'Root' ? undefined : tree.nodes.get(nodeKey(ancestor.shownUnder));
    }
  }
  if (before !== null) {
    const anchor = tree.nodes.get(nodeKey(before));
    if (!anchor || before._tag !== node._tag || nodeKey(before) === nodeKey(node) || !sameParent(anchor.shownUnder, parent)) {
      return refuse({ _tag: 'AnchorMoved', before, parent, ...(anchor ? { foundUnder: anchor.shownUnder } : {}) });
    }
  }
  if (sameParent(entry.parent, parent) && sameParent(entry.shownUnder, parent)) {
    const siblings = parent._tag === 'Root' ? tree.roots : tree.nodes.get(nodeKey(parent))!.children;
    let next: NodeRef | null = null;
    const index = siblings.findIndex(sibling => nodeKey(sibling) === nodeKey(node));
    for (let i = index + 1; i < siblings.length; i++) {
      if (siblings[i]!._tag === node._tag) { next = siblings[i]!; break; }
    }
    if (next === null ? before === null : before !== null && nodeKey(next) === nodeKey(before)) return refuse({ _tag: 'NoChange', node, parent });
  }
  return { _tag: 'Planned', intent };
};

export const canPlace = (tree: SidebarTree, intent: MoveIntent): PlacementCheck => {
  const result = planMove(tree, intent);
  return result._tag === 'Planned' ? { _tag: 'Allowed' } : result;
};

/** Count each descendant subject once; subject leaves have no rollup entry. */
export const rollup = (tree: SidebarTree, needsYou: ReadonlySet<string>): Map<NodeKey, Rollup> => {
  const result = new Map<NodeKey, Rollup>();
  const pending = [...tree.roots];
  const folders: ProjectedNode[] = [];
  while (pending.length > 0) {
    const node = pending.pop()!;
    const entry = tree.nodes.get(nodeKey(node))!;
    if (node._tag === 'Folder') {
      folders.push(entry);
      for (const child of entry.children) pending.push(child);
    }
  }
  for (let i = folders.length - 1; i >= 0; i--) {
    const folder = folders[i]!;
    let count = 0;
    for (const child of folder.children) {
      count += child._tag === 'Subject' ? Number(needsYou.has(child.subject)) : result.get(nodeKey(child))!.needsYou;
    }
    result.set(nodeKey(folder.node), { needsYou: count, display: 'Always' });
  }
  return result;
};
