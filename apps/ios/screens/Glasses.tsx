import { useLayoutEffect, useMemo } from 'react';
import { ScrollView, StyleSheet, View } from 'react-native';
import { useNavigation } from '@react-navigation/native';
import type { NativeStackNavigationProp } from '@react-navigation/native-stack';
import { Banners, Empty, StatusLine, useListsOnFocus, useMissionsOnFocus } from '../chrome';
import { glassChoices, glassGroups, groupBoxes, spaceSummary, type GlassLists, type PaneTarget } from '../glassesView';
import { navigationRef, type RootParams, type RootScreen } from '../navigation';
import { useStore } from '../store';
import { theme } from '../theme';
import { ListRow, Note, Screen, T } from '../ui';

// Spaces on the phone: the list of the person's spaces by name, and each one's tabs across all its
// panes, grouped as the panes group them on the computer and in the same order. A tab opens what it
// shows. Both follow st live, so a change made in stui shows here.

type Navigation = NativeStackNavigationProp<RootParams>;

/** Open what a pane shows, in this tab's stack where the app has a screen for it. */
function openPane(navigation: Navigation, pane: PaneTarget) {
  if (pane.gone) return;
  if (pane.kind === 'agent') navigation.navigate('Conversation', { target: pane.id, title: pane.title });
  else if (pane.kind === 'mission') navigation.navigate('Mission', { id: pane.id, title: pane.title });
  else if (pane.kind === 'terminal') navigation.navigate('Terminal', { terminalId: pane.id, title: pane.title });
  else if (pane.kind === 'usage') navigation.navigate('Usage');
  else if (pane.kind === 'machine') navigationRef.navigate('Fleet', { screen: 'FleetRoot' });
}

function paneGlyph(pane: PaneTarget): { glyph: string; color: string } {
  if (pane.gone) return { glyph: '○', color: theme.overlay1 };
  if (pane.needsYou) return { glyph: '◆', color: theme.person };
  // Agents and missions are marked as in their own lists: working, idle, broken…
  if (pane.status) return pane.status;
  const glyphs: Record<PaneTarget['kind'], string> = { agent: '●', mission: '◇', machine: '▣', terminal: '⌨', usage: '$', other: '·' };
  return { glyph: glyphs[pane.kind], color: theme.subtext0 };
}

function paneDetail(pane: PaneTarget): string {
  if (pane.gone) return 'gone';
  if (pane.needsYou) return `${pane.kind} · alert`;
  if (pane.status) return `${pane.kind} · ${pane.status.word}`;
  return pane.kind === 'other' ? 'this app cannot show it yet' : pane.kind;
}

function useLists(): GlassLists {
  const { data } = useStore();
  return useMemo(() => ({ agents: data.agents, missions: data.missions, attention: data.attention, machines: data.machines }), [data]);
}

function Notes() {
  const { glassesGranted, glassesIssue } = useStore();
  return <>
    {!glassesGranted ? <Note tone="warning">This gateway does not keep spaces yet, or this device was paired before it could. Spaces made in stui appear here once it does.</Note> : null}
    {glassesIssue ? <Note tone="fault">{glassesIssue}</Note> : null}
  </>;
}

/** The person's spaces, by name. */
export function GlassesScreen() {
  const { glasses, glassesGranted } = useStore();
  const navigation = useNavigation<Navigation>();
  const choices = glassChoices(glasses);
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ paddingBottom: 32 }}>
      <StatusLine />
      <Notes />
      {choices.map(choice => {
        const glass = glasses.find(candidate => candidate.id === choice.id)!;
        return <ListRow key={choice.id} glyph="▢" glyphColor={theme.lavender} title={choice.name} second={spaceSummary(glass)}
          onPress={() => navigation.navigate('Space', { id: choice.id, title: choice.name })} />;
      })}
      {!choices.length && glassesGranted ? <Empty text="No spaces yet. Make one in stui on a computer." /> : null}
    </ScrollView>
  </Screen>;
}

/** One space: every tab across its panes, each pane's tabs together as on the computer. */
export function SpaceScreen({ route, navigation }: RootScreen<'Space'>) {
  const { glasses } = useStore();
  useListsOnFocus(['machines']);
  useMissionsOnFocus();
  const lists = useLists();
  const glass = glasses.find(candidate => candidate.id === route.params.id);
  useLayoutEffect(() => { navigation.setOptions({ title: glass?.body?.name ?? route.params.title ?? 'Space' }); }, [navigation, glass?.body?.name, route.params.title]);
  const panes = glass ? glassGroups(glass, lists) : [];
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ paddingVertical: 8, paddingBottom: 32 }}>
      <Notes />
      {!glass ? <Empty text="This space is gone." /> : null}
      {glass?.body && panes.length > 1 ? <SpaceMap layout={glass.body.layout} titles={panes.map(pane => pane.tabs[0]?.title ?? '')} /> : null}
      {panes.map(pane => <View key={pane.index} style={styles.pane}>
        {pane.tabs.map(tab => {
          const glyph = paneGlyph(tab.pane);
          return <ListRow key={`${tab.group}-${tab.index}`} glyph={glyph.glyph} glyphColor={glyph.color} title={tab.title} second={paneDetail(tab.pane)}
            onPress={tab.pane.gone ? undefined : () => openPane(navigation as unknown as Navigation, tab.pane)} />;
        })}
        {!pane.tabs.length ? <T dim style={{ paddingHorizontal: 12, paddingVertical: 8 }}>Empty on the computer too.</T> : null}
      </View>)}
    </ScrollView>
  </Screen>;
}

/** The space's splits at the sizes stui gives them (st keeps them from glasses version 2), each
 * labelled with its first tab, so the cards below map to where they sit on the computer. */
function SpaceMap({ layout, titles }: { layout: Parameters<typeof groupBoxes>[0]; titles: string[] }) {
  return <View style={styles.map}>
    {groupBoxes(layout).map((box, index) => <View key={index} style={[styles.mapBox, { left: `${box.x * 100}%`, top: `${box.y * 100}%`, width: `${box.width * 100}%`, height: `${box.height * 100}%` }]}>
      <T dim numberOfLines={2} style={{ fontSize: 11, lineHeight: 14 }}>{titles[index] || 'empty'}</T>
    </View>)}
  </View>;
}

// A pane's tabs share a card, with space between cards: grouped without headings.
const styles = StyleSheet.create({
  map: { marginHorizontal: 8, marginVertical: 6, aspectRatio: 16 / 9, position: 'relative' },
  mapBox: { position: 'absolute', borderWidth: StyleSheet.hairlineWidth * 2, borderColor: theme.surface1, backgroundColor: theme.mantle, padding: 4, overflow: 'hidden' },
  pane: { marginHorizontal: 8, marginVertical: 6, borderRadius: 10, backgroundColor: theme.mantle, borderLeftWidth: 2, borderLeftColor: theme.surface1, overflow: 'hidden' },
});
