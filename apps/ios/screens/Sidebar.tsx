import { useCallback, useEffect, useRef, useState } from 'react';
import { ActionSheetIOS, Alert, Modal, PanResponder, Pressable, RefreshControl, ScrollView, View } from 'react-native';
import { useFocusEffect } from '@react-navigation/native';
import AsyncStorage from '@react-native-async-storage/async-storage';
import * as Crypto from 'expo-crypto';
import { openAlerts } from '@smalltalk/st3-views';
import type { ArrangementOperation } from '../../../clients/typescript/st3-client';
import { Banners, StatusLine, useListsOnFocus, useMissionsOnFocus } from '../chrome';
import { agentGlyph, agentRows } from '../agentsView';
import { missionTitle, missionWord, wordStyle } from '../missionsView';
import { navigationRef, type RootScreen } from '../navigation';
import { rememberResources, sidebarGroups, type SidebarChoices, type SidebarResource } from '../sidebarView';
import { resourceRow, subjectToFile } from '../sidebarResources';
import { folderCreate, folderName, folderLocation, moveFolder, placeResource, sidebarArrangement, uuid7 } from '../sidebarEdits';
import { useStore } from '../store';
import { Button, Field, ListRow, Note, Screen, T } from '../ui';
import { theme } from '../theme';

type DragTarget = { id: string; folder: string | null; resource?: SidebarResource; title: string };
type Form = { kind: 'new'; subject?: string } | { kind: 'rename'; id: string; title: string } | { kind: 'add'; id: string; title: string } | { kind: 'place'; resource: SidebarResource; folder: string | null; title: string };
/** A handle owns the gesture; tapping it never moves a row. */
function DragHandle({ title, onBegin, onEnd, onDrop }: { title: string; onBegin: () => void; onEnd: () => void; onDrop: (y: number) => void }) {
  const latest = useRef({ onBegin, onEnd, onDrop }); latest.current = { onBegin, onEnd, onDrop };
  const moved = useRef(false);
  const responder = useRef(PanResponder.create({
    onStartShouldSetPanResponder: () => true,
    onPanResponderGrant: () => { moved.current = false; latest.current.onBegin(); },
    onPanResponderMove: (_, gesture) => { moved.current ||= Math.abs(gesture.dy) > 6 || Math.abs(gesture.dx) > 6; },
    onPanResponderRelease: (_, gesture) => { if (moved.current) latest.current.onDrop(gesture.moveY); latest.current.onEnd(); },
    onPanResponderTerminate: () => latest.current.onEnd(),
  })).current;
  return <View {...responder.panHandlers} accessibilityLabel={`Drag ${title} to move or reorder`} style={{ padding: 14 }}><T dim>≡</T></View>;
}

export function SidebarScreen({ navigation }: RootScreen<'ResourcesRoot'>) {
  const { data, arrangements, arrangementsIssue, sidebarTerminals, sidebarTerminalsIssue, loadSidebarTerminals, watchSidebarTerminals, sidebarResources, sidebarResourcesIssue, loadSidebarResources, loadLists, sidebarSeen, url, caps, actor, status, hasSynced, busy, actions, loadErrors, truncated, gatewayHost, error, feed, inventoryGranted } = useStore();
  useListsOnFocus(['machines', 'sessions']); useMissionsOnFocus();
  useFocusEffect(useCallback(() => {
    if (inventoryGranted) return watchSidebarTerminals();
    if (inventoryGranted === false && status === 'online') void loadSidebarTerminals();
  }, [status, loadSidebarTerminals, watchSidebarTerminals, inventoryGranted]));
  useFocusEffect(useCallback(() => { if (status === 'online') void loadSidebarResources(); }, [status, loadSidebarResources]));
  const [refreshing, setRefreshing] = useState(false);
  const refresh = <RefreshControl refreshing={refreshing} tintColor={theme.overlay1} onRefresh={() => {
    setRefreshing(true); if (status !== 'online') actions.reconnect(); else if (inventoryGranted) feed?.refresh();
    void Promise.all([loadLists(['machines', 'sessions']), loadSidebarTerminals(), loadSidebarResources()]).finally(() => setRefreshing(false));
  }} />;
  const [filter, setFilter] = useState(''), [choices, setChoices] = useState<SidebarChoices>({}), [loadedKey, setLoadedKey] = useState('');
  const [form, setForm] = useState<Form | null>(null), [name, setName] = useState(''), [issue, setIssue] = useState(''), [dragging, setDragging] = useState(false);
  const scroll = useRef<ScrollView>(null), scrollY = useRef(0), screenY = useRef(0), positions = useRef(new Map<string, { y: number; height: number }>());
  const storageKey = `st3.sidebar.collapse/${url}/${actor.match(/^person\/[^/]+/)?.[0] ?? ''}`;
  useEffect(() => {
    let current = true; setChoices({}); setLoadedKey('');
    void AsyncStorage.getItem(storageKey).then(value => {
      if (!current) return;
      try {
        const parsed = value ? JSON.parse(value) : {}, collapsed: unknown = parsed.collapsed ?? parsed;
        setChoices(collapsed && typeof collapsed === 'object' ? Object.fromEntries(Object.entries(collapsed).filter(([, value]) => typeof value === 'boolean')) : {});
        if (Array.isArray(parsed.resources)) {
          const saved = parsed.resources.filter((row: unknown): row is SidebarResource => !!row && typeof row === 'object'
            && typeof (row as SidebarResource).id === 'string' && typeof (row as SidebarResource).title === 'string'
            && typeof (row as SidebarResource).target === 'string' && ['Agents', 'Missions', 'Terminals', 'Machines', 'Pull requests', 'Repositories', 'Resources', 'Unavailable'].includes((row as SidebarResource).kind));
          sidebarSeen.current = new Map([...saved.map((row: SidebarResource) => [row.id, { ...row, missing: true }] as const), ...sidebarSeen.current]);
        }
      } catch { setChoices({}); }
      setLoadedKey(storageKey);
    }).catch(() => { if (current) setLoadedKey(storageKey); });
    return () => { current = false; };
  }, [storageKey]);
  const agents = agentRows(data.agents, data.sessions, gatewayHost || '?', Date.now(), data.missions);
  const current: SidebarResource[] = [
    ...sidebarResources.map(resourceRow),
    ...agents.map(agent => ({ id: agent.id, title: agent.name, kind: 'Agents' as const, target: agent.target })),
    ...data.missions.map(mission => ({ id: mission.id, title: missionTitle(mission), kind: 'Missions' as const, target: mission.id })),
    ...sidebarTerminals.map(terminal => ({ id: terminal.id, title: terminal.owner_id, kind: 'Terminals' as const, target: terminal.terminal_id! })),
    ...data.machines.map(machine => ({ id: machine.id, title: machine.name, kind: 'Machines' as const, target: machine.id })),
  ];
  sidebarSeen.current = rememberResources(sidebarSeen.current, current);
  const view = sidebarGroups(arrangements, sidebarSeen.current, choices, filter), arrangement = sidebarArrangement(arrangements);
  const editable = status === 'online' && !busy && (caps?.capabilities.some(c => c.id === 'arrangements' && c.state === 'granted') ?? false);
  useEffect(() => {
    if (loadedKey === storageKey) void AsyncStorage.setItem(storageKey, JSON.stringify({ collapsed: choices, resources: [...sidebarSeen.current.values()] })).catch(() => {});
  }, [loadedKey, storageKey, choices, data.agents, data.sessions, data.missions, data.machines, sidebarTerminals, sidebarResources, arrangements]);
  const toggle = (id: string, collapsed: boolean) => setChoices(value => ({ ...value, [id]: !collapsed }));
  const submit = async (build: () => ArrangementOperation[]) => {
    if (!editable) return false;
    try { setIssue(''); return await actions.editSidebar(build()); } catch (error) { setIssue(error instanceof Error ? error.message : String(error)); return false; }
  };
  const openForm = (value: Form) => { setName(value.kind === 'rename' ? value.title : ''); setForm(value); };
  const makeFolder = async () => {
    if (!form || !name.trim()) return;
    if (form.kind === 'rename') { if (await submit(() => [{ op: 'folder.rename', id: form.id, name: folderName(name) }])) setForm(null); }
    else if (form.kind === 'new') {
      const id = uuid7(Date.now(), Crypto.getRandomBytes(10));
      if (await submit(() => [...folderCreate(arrangement, id, name), ...(form.subject ? [{ op: 'subject.place' as const, subject: form.subject, folder: id, key: 'a0' }] : [])])) setForm(null);
    }
  };
  const open = (row: SidebarResource) => {
    if (row.missing) return;
    if (row.kind === 'Agents') navigation.navigate('Conversation', { target: row.target, title: row.title });
    else if (row.kind === 'Missions') navigation.navigate('Mission', { id: row.target, title: row.title });
    else if (row.kind === 'Terminals') navigation.navigate('Terminal', { terminalId: row.target, title: row.title });
    else if (row.kind === 'Machines') navigationRef.navigate('Fleet', { screen: 'FleetRoot' });
    else navigation.navigate('Resource', { id: row.id, title: row.title });
  };
  const move = (row: SidebarResource) => {
    const folders = view.groups.filter(g => g.folder);
    ActionSheetIOS.showActionSheetWithOptions({ title: `Move ${row.title} to…`, options: [...folders.map(g => g.title), 'Everything else', 'New folder…', 'Cancel'], cancelButtonIndex: folders.length + 2 }, index => {
      if (index >= 0 && index < folders.length) openForm({ kind: 'place', resource: row, folder: folders[index].id.slice(7), title: folders[index].title });
      else if (index === folders.length) openForm({ kind: 'place', resource: row, folder: null, title: 'Everything else' });
      else if (index === folders.length + 1) openForm({ kind: 'new', subject: row.id });
    });
  };
  const folderMenu = (id: string, title: string) => ActionSheetIOS.showActionSheetWithOptions({ title, options: ['Rename…', 'Delete folder…', 'Cancel'], cancelButtonIndex: 2, destructiveButtonIndex: 1 }, index => {
    if (index === 0) openForm({ kind: 'rename', id, title });
    if (index === 1) Alert.alert(`Delete “${title}”?`, 'Its resources return to Everything else.', [{ text: 'Cancel', style: 'cancel' }, { text: 'Delete folder', style: 'destructive', onPress: () => void submit(() => [{ op: 'folder.delete', id }]) }]);
  });
  const needs = new Set(openAlerts(data.attention, caps?.session_actor).flatMap(a => [a.source_id, a.requester_id, a.mission_id].filter((id): id is string => !!id)));
  const agentsById = new Map(agents.map(agent => [agent.id, agent])), missionsById = new Map(data.missions.map(mission => [mission.id, mission]));
  const mark = (row: SidebarResource) => {
    const agent = agentsById.get(row.id), mission = missionsById.get(row.id);
    return agent ? agentGlyph(agent.state) : mission ? wordStyle(missionWord(mission)) : { glyph: row.missing ? '?' : '·', color: theme.overlay0 };
  };
  const targets: DragTarget[] = [];
  for (const group of view.groups.filter(g => g.folder)) {
    const folder = group.id.slice(7); targets.push({ id: group.id, folder, title: group.title });
    if (!group.collapsed) targets.push(...group.resources.map(resource => ({ id: resource.id, folder, resource, title: resource.title })));
  }
  targets.push({ id: 'everything', folder: null, title: 'Everything else' });
  if (!view.everythingCollapsed) for (const group of view.groups.filter(g => !g.folder && !g.collapsed)) targets.push(...group.resources.map(resource => ({ id: resource.id, folder: null, resource, title: resource.title })));
  const drop = (source: DragTarget, y: number) => {
    const at = y - screenY.current + scrollY.current;
    const candidates = targets.filter(target => target.id !== source.id && (source.resource || target.id.startsWith('folder/')));
    const target = candidates.find(target => { const p = positions.current.get(target.id); return p && at >= p.y && at <= p.y + p.height; });
    if (!target) return;
    const p = positions.current.get(target.id)!;
    if (!source.resource) {
      const folders = targets.filter(t => t.id.startsWith('folder/')); const after = at > p.y + p.height / 2;
      const anchor = after ? folders[folders.findIndex(t => t.id === target.id) + 1]?.id.slice(7) : target.id.slice(7);
      if (anchor !== source.id.slice(7) && arrangement) void submit(() => moveFolder(arrangement, source.id.slice(7), anchor));
    } else {
      const after = !!target.resource && at > p.y + p.height / 2;
      void submit(() => placeResource(arrangement, source.resource!.id, target.folder, after ? undefined : target.resource?.id, after ? target.resource?.id : undefined));
    }
  };
  const handle = (target: DragTarget) => editable && !/^(session|pty)\//.test(target.id) ? <DragHandle title={target.title} onBegin={() => { setDragging(true); scroll.current?.getNativeScrollRef()?.measureInWindow((_: number, y: number) => { screenY.current = y; }); }} onEnd={() => setDragging(false)} onDrop={y => drop(target, y)} /> : null;
  const row = (resource: SidebarResource, folder: string | null) => {
    const target = { id: resource.id, folder, resource, title: resource.title }, glyph = mark(resource);
    return <View key={resource.id} onLayout={e => positions.current.set(resource.id, e.nativeEvent.layout)} style={{ flexDirection: 'row', alignItems: 'center' }}>
      <View style={{ flex: 1 }}><ListRow glyph={glyph.glyph} glyphColor={glyph.color} title={resource.title} right={needs.has(resource.id) ? <T color={theme.person}>●</T> : undefined} second={resource.missing ? `${resource.id} · unavailable` : resource.kind} onPress={resource.missing ? undefined : () => open(resource)} onLongPress={editable && !/^(session|pty)\//.test(resource.id) ? () => move(resource) : undefined} /></View>{handle(target)}
    </View>;
  };
  const heading = (id: string, title: string, count: number, collapsed: boolean, folder?: string) => <View key={id} onLayout={e => positions.current.set(id, e.nativeEvent.layout)} style={{ flexDirection: 'row', alignItems: 'center' }}>
    <Pressable accessibilityRole="button" accessibilityState={{ expanded: !collapsed }} accessibilityLabel={`${title}, ${count} items`} onPress={() => toggle(id, collapsed)} onLongPress={folder && editable ? () => folderMenu(folder, title) : undefined} style={{ padding: 12, flex: 1 }}><T bold>{collapsed ? '▸' : '▾'} {title} <T dim>{count}</T></T></Pressable>
    {folder && editable ? <Button label="+" onPress={() => openForm({ kind: 'add', id: folder, title })} /> : null}
    {folder ? handle({ id, folder, title }) : null}
  </View>;
  return <Screen><Banners /><ScrollView ref={scroll} contentInsetAdjustmentBehavior="automatic" refreshControl={refresh} scrollEnabled={!dragging} onScroll={e => { scrollY.current = e.nativeEvent.contentOffset.y; }} scrollEventThrottle={16}>
    <View style={{ padding: 12 }}><Field accessibilityLabel="Filter sidebar" value={filter} onChangeText={setFilter} placeholder="Find a resource" /><Button label="New folder" disabled={!editable} onPress={() => openForm({ kind: 'new' })} /></View>
    <StatusLine />
    <ListRow glyph="●" glyphColor={theme.idle} title="Agents" second="Search agents and conversations · list or tree" onPress={() => navigation.navigate('Agents')} />
    <ListRow glyph="◇" glyphColor={theme.waiting} title="Missions" second="Search missions · steps, progress and launches" onPress={() => navigation.navigate('Missions')} />
    {issue ? <Note tone="warning">{issue}</Note> : null}
    {arrangementsIssue ? <Note tone="warning">{arrangementsIssue}</Note> : null}
    {sidebarResourcesIssue ? <Note tone="warning">{sidebarResourcesIssue}</Note> : null}
    {sidebarTerminalsIssue ? <Note tone="warning">{sidebarTerminalsIssue}</Note> : null}
    {loadErrors.missions ? <Note tone="warning">{loadErrors.missions}</Note> : null}
    {truncated.agents || truncated.missions ? <Note tone="warning">More resources exist beyond this window.</Note> : null}
    {view.groups.filter(g => g.folder).flatMap(group => [heading(group.id, group.title, group.resources.length, group.collapsed, group.id.slice(7)), ...(!group.collapsed ? group.resources.map(resource => row(resource, group.id.slice(7))) : [])])}
    {heading('everything', 'Everything else', view.unfiledCount, view.everythingCollapsed)}
    {!view.everythingCollapsed && view.groups.filter(g => !g.folder).flatMap(group => [heading(group.id, group.title, group.resources.length, group.collapsed), ...(!group.collapsed ? group.resources.map(resource => row(resource, null)) : [])])}
    <Button label="Past sessions" onPress={() => navigation.navigate('History')} />
    {[...sidebarSeen.current.values()].every(resource => resource.kind === 'Machines') && !arrangements.length ? <Note>{hasSynced ? 'Welcome. Use New on Now, then New terminal or New mission to start.' : 'Loading your resources…'}</Note> : null}
  </ScrollView>
    <Modal visible={!!form} transparent animationType="slide" onRequestClose={() => setForm(null)}><View style={{ flex: 1, justifyContent: 'center', padding: 24, backgroundColor: '#0008' }}><View style={{ backgroundColor: theme.mantle, padding: 16 }}>
      {issue || error ? <Note tone="fault">{issue || error}</Note> : null}
      {form?.kind === 'place' ? <><T bold>Move {form.resource.title} to {form.title}?</T><Button label="Move" disabled={!editable} onPress={() => void submit(() => placeResource(arrangement, form.resource.id, form.folder)).then(done => { if (done) setForm(null); })} /></> : <><T bold>{form?.kind === 'add' ? `Add to ${form.title}` : form?.kind === 'rename' ? `Rename ${form.title}` : 'New folder'}</T>
      <Field accessibilityLabel={form?.kind === 'add' ? 'Find a resource to add' : 'Folder name'} value={name} onChangeText={setName} autoFocus onSubmitEditing={form?.kind !== 'add' ? () => void makeFolder() : undefined} />
      {form?.kind === 'add' ? <ScrollView style={{ maxHeight: 360 }}>{subjectToFile(name, sidebarSeen.current) ? <ListRow title={`File ${name.trim()}`} onPress={editable ? () => openForm({ kind: 'place', resource: subjectToFile(name, sidebarSeen.current)!, folder: form.id, title: form.title }) : undefined} /> : null}{[...sidebarSeen.current.values()].filter(resource => !resource.missing && !/^(session|pty)\//.test(resource.id) && folderLocation(arrangement, resource.id) !== form.id && (resource.title.toLowerCase().includes(name.toLowerCase()) || resource.id.toLowerCase().includes(name.toLowerCase()))).map(resource => <ListRow key={resource.id} title={resource.title} onPress={editable ? () => openForm({ kind: 'place', resource, folder: form.id, title: form.title }) : undefined} />)}</ScrollView> : <Button label="Save" disabled={!editable || !name.trim()} onPress={() => void makeFolder()} />}</>}
      <Button label="Cancel" onPress={() => setForm(null)} />
    </View></View></Modal>
  </Screen>;
}
