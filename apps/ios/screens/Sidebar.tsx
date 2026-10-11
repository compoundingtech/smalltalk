import { useCallback, useEffect, useState } from 'react';
import { Pressable, ScrollView, View } from 'react-native';
import { useFocusEffect } from '@react-navigation/native';
import AsyncStorage from '@react-native-async-storage/async-storage';
import { Banners, StatusLine, useListsOnFocus, useRefresh } from '../chrome';
import { agentName } from '../agentsView';
import { missionTitle } from '../missionsView';
import type { RootScreen } from '../navigation';
import { rememberResources, sidebarGroups, type SidebarChoices, type SidebarResource } from '../sidebarView';
import { useStore } from '../store';
import { Field, ListRow, Note, Screen, T } from '../ui';
import { theme } from '../theme';

/** This phone's resource browser, alongside Home and Spaces. Opening only navigates. */
export function SidebarScreen({ navigation }: RootScreen<'Sidebar'>) {
  const { data, arrangements, arrangementsIssue, sidebarTerminals, sidebarTerminalsIssue, loadSidebarTerminals, sidebarSeen, url, caps, status, hasSynced } = useStore();
  useListsOnFocus(['machines']);
  useFocusEffect(useCallback(() => { if (status === 'online') void loadSidebarTerminals(); }, [status, loadSidebarTerminals]));
  const refresh = useRefresh(['machines']);
  const [filter, setFilter] = useState(''), [choices, setChoices] = useState<SidebarChoices>({}), [loadedKey, setLoadedKey] = useState('');
  const storageKey = `st3.sidebar.collapse/${url}/${caps?.session_actor.match(/^person\/[^/]+/)?.[0] ?? ''}`;
  useEffect(() => {
    let current = true;
    setChoices({}); setLoadedKey('');
    void AsyncStorage.getItem(storageKey).then(value => {
      if (!current) return;
      try {
        const parsed = value ? JSON.parse(value) : {};
        const collapsed: unknown = parsed.collapsed ?? parsed;
        setChoices(collapsed && typeof collapsed === 'object' ? Object.fromEntries(Object.entries(collapsed).filter(([, value]) => typeof value === 'boolean')) : {});
        if (Array.isArray(parsed.resources)) {
          const saved = parsed.resources.filter((row: unknown): row is SidebarResource => !!row && typeof row === 'object'
            && typeof (row as SidebarResource).id === 'string' && typeof (row as SidebarResource).title === 'string'
            && typeof (row as SidebarResource).target === 'string' && ['Agents', 'Missions', 'Terminals', 'Machines', 'Unavailable'].includes((row as SidebarResource).kind));
          // Fresh stream rows win over stored names and availability.
          sidebarSeen.current = new Map([...saved.map((row: SidebarResource) => [row.id, { ...row, missing: true }] as const), ...sidebarSeen.current]);
        }
      } catch { setChoices({}); }
      setLoadedKey(storageKey);
    }).catch(() => { if (current) setLoadedKey(storageKey); });
    return () => { current = false; };
  }, [storageKey]);
  const current: SidebarResource[] = [
    ...data.agents.map(agent => ({ id: agent.id, title: agentName(agent), kind: 'Agents' as const, target: agent.id })),
    ...data.missions.map(mission => ({ id: mission.id, title: missionTitle(mission), kind: 'Missions' as const, target: mission.id })),
    ...sidebarTerminals.map(terminal => ({ id: terminal.id, title: terminal.owner_id, kind: 'Terminals' as const, target: terminal.terminal_id! })),
    ...data.machines.map(machine => ({ id: machine.id, title: machine.name, kind: 'Machines' as const, target: machine.id })),
  ];
  sidebarSeen.current = rememberResources(sidebarSeen.current, current);
  const view = sidebarGroups(arrangements, sidebarSeen.current, choices, filter);
  useEffect(() => {
    if (loadedKey === storageKey) void AsyncStorage.setItem(storageKey, JSON.stringify({ collapsed: choices, resources: [...sidebarSeen.current.values()] })).catch(() => {});
  }, [loadedKey, storageKey, choices, data.agents, data.missions, data.machines, sidebarTerminals, arrangements]);
  const toggle = (id: string, collapsed: boolean) => {
    const next = { ...choices, [id]: !collapsed };
    setChoices(next);

  };
  const open = (row: SidebarResource) => {
    if (row.missing) return;
    if (row.kind === 'Agents') navigation.navigate('Conversation', { target: row.target, title: row.title });
    else if (row.kind === 'Missions') navigation.navigate('Mission', { id: row.target, title: row.title });
    else if (row.kind === 'Terminals') navigation.navigate('Terminal', { terminalId: row.target, title: row.title });
    else if (row.kind === 'Machines') navigation.navigate('FleetRoot');
  };
  const heading = (id: string, title: string, count: number, collapsed: boolean) => <Pressable accessibilityRole="button" accessibilityState={{ expanded: !collapsed }} accessibilityLabel={`${title}, ${count} items`} onPress={() => toggle(id, collapsed)} style={{ padding: 12 }}><T bold>{collapsed ? '▸' : '▾'} {title} <T dim>{count}</T></T></Pressable>;
  return <Screen><Banners /><ScrollView contentInsetAdjustmentBehavior="automatic" refreshControl={refresh}>
    <StatusLine />
    <View style={{ padding: 12 }}><Field accessibilityLabel="Filter sidebar" value={filter} onChangeText={setFilter} placeholder="Find a resource" /><Pressable onPress={() => setFilter('')}><T color={theme.accent}>Clear filter</T></Pressable></View>
    {arrangementsIssue ? <Note tone="warning">{arrangementsIssue}</Note> : null}
    {sidebarTerminalsIssue ? <Note tone="warning">{sidebarTerminalsIssue}</Note> : null}
    {view.groups.filter(group => group.folder).map(group => <View key={group.id}>
      {heading(group.id, group.title, group.resources.length, group.collapsed)}
      {!group.collapsed && group.resources.map(row => <ListRow key={row.id} title={row.title} second={row.missing ? `${row.id} · unavailable` : row.kind} onPress={row.missing ? undefined : () => open(row)} />)}
    </View>)}
    {heading('everything', 'Everything else', view.unfiledCount, view.everythingCollapsed)}
    {!view.everythingCollapsed && view.groups.filter(group => !group.folder).map(group => <View key={group.id}>
      {heading(group.id, group.title, group.resources.length, group.collapsed)}
      {!group.collapsed && group.resources.map(row => <ListRow key={row.id} title={row.title} second={row.missing ? `${row.id} · unavailable` : undefined} onPress={row.missing ? undefined : () => open(row)} />)}
    </View>)}
    {[...sidebarSeen.current.values()].every(row => row.kind === 'Machines') && !arrangements.length ? <Note>{hasSynced ? 'Welcome. Use New on Home, then New terminal or New agent to start.' : 'Loading your resources…'}</Note> : null}
  </ScrollView></Screen>;
}
