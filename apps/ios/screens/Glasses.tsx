import { useMemo, useState } from 'react';
import { ScrollView, View } from 'react-native';
import { useNavigation } from '@react-navigation/native';
import type { NativeStackNavigationProp } from '@react-navigation/native-stack';
import SegmentedControl from '@react-native-segmented-control/segmented-control';
import { Banners, Empty, StatusLine, useListsOnFocus } from '../chrome';
import { glassChoices, glassTabs, type GlassLists, type PaneTarget } from '../glassesView';
import { navigationRef, type RootParams, type RootScreen } from '../navigation';
import { useStore } from '../store';
import { theme } from '../theme';
import { ListRow, Note, Screen, SectionHeader, T } from '../ui';

// Glasses on the phone (an experiment, on in Fleet): one thing at a time. The root lists the
// chosen glass's tabs, Home first; a tab with one pane opens it, a split tab lists its panes.

type Navigation = NativeStackNavigationProp<RootParams>;

/** Open what a pane shows, in this tab's stack where the app has a screen for it. */
function openPane(navigation: Navigation, pane: PaneTarget) {
  if (pane.gone) return;
  if (pane.kind === 'agent') navigation.navigate('Conversation', { target: pane.id, title: pane.title });
  else if (pane.kind === 'mission') navigation.navigate('Mission', { id: pane.id, title: pane.title });
  else if (pane.kind === 'machine') navigationRef.navigate('Fleet', { screen: 'FleetRoot' });
}

function paneGlyph(pane: PaneTarget): { glyph: string; color: string } {
  if (pane.gone) return { glyph: '○', color: theme.overlay1 };
  if (pane.needsYou) return { glyph: '◆', color: theme.person };
  return { glyph: pane.kind === 'agent' ? '●' : pane.kind === 'mission' ? '◇' : pane.kind === 'machine' ? '▣' : '·', color: theme.subtext0 };
}

function paneDetail(pane: PaneTarget): string {
  if (pane.gone) return 'gone';
  return pane.kind === 'other' ? 'this app cannot show it yet' : pane.kind;
}

function useLists(): GlassLists {
  const { data } = useStore();
  return useMemo(() => ({ agents: data.agents, missions: data.missions, attention: data.attention, machines: data.machines }), [data]);
}

export function GlassesScreen() {
  const { glasses, glassesGranted, glassesIssue } = useStore();
  const navigation = useNavigation<Navigation>();
  useListsOnFocus(['machines']);
  const lists = useLists();
  const choices = glassChoices(glasses);
  const [chosen, setChosen] = useState<string | null>(null);
  const shownId = choices.find(choice => choice.id === chosen)?.id ?? choices.find(choice => choice.name === 'main')?.id ?? choices[0]?.id;
  const shown = glasses.find(glass => glass.id === shownId);
  const tabs = shown ? glassTabs(shown, lists) : [];
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ paddingBottom: 32 }}>
      <StatusLine />
      {!glassesGranted ? <Note tone="warning">This gateway does not keep glasses yet, or this device was paired before it could. Glasses made in stui --glasses appear here once it does.</Note> : null}
      {glassesIssue ? <Note tone="fault">{glassesIssue}</Note> : null}
      {choices.length > 1 ? <View style={{ paddingHorizontal: 12, paddingVertical: 8 }}>
        <SegmentedControl
          values={choices.map(choice => choice.name)}
          selectedIndex={Math.max(0, choices.findIndex(choice => choice.id === shownId))}
          onChange={event => setChosen(choices[event.nativeEvent.selectedSegmentIndex]?.id ?? null)}
        />
      </View> : null}
      {shown ? <>
        <SectionHeader title={shown.body?.name ?? 'glass'} count={tabs.length + 1} />
        <ListRow glyph="⌂" glyphColor={theme.accent} title="Home" second="what needs you" onPress={() => navigationRef.navigate('Home', { screen: 'HomeRoot' })} />
        {tabs.map(tab => {
          const only = tab.panes.length === 1 ? tab.panes[0] : null;
          const glyph = only ? paneGlyph(only) : { glyph: tab.needsYou ? '◆' : '▤', color: tab.needsYou ? theme.person : theme.subtext0 };
          return <ListRow
            key={tab.index}
            glyph={glyph.glyph}
            glyphColor={glyph.color}
            title={tab.title}
            second={only ? paneDetail(only) : `${tab.panes.length} panes`}
            onPress={() => only ? openPane(navigation, only) : navigation.navigate('GlassTab', { glass: shown.id, index: tab.index })}
          />;
        })}
      </> : glassesGranted ? <Empty text="No glasses yet. Make one with stui --glasses on a computer." /> : null}
    </ScrollView>
  </Screen>;
}

export function GlassTabScreen({ route }: RootScreen<'GlassTab'>) {
  const { glasses } = useStore();
  const navigation = useNavigation<Navigation>();
  const lists = useLists();
  const glass = glasses.find(candidate => candidate.id === route.params.glass);
  const tab = glass ? glassTabs(glass, lists)[route.params.index] : undefined;
  if (!tab) return <Screen><Empty text="This tab is no longer in its glass." /></Screen>;
  return <Screen>
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ paddingBottom: 32 }}>
      <SectionHeader title={tab.title} count={tab.panes.length} />
      {tab.panes.map(pane => {
        const glyph = paneGlyph(pane);
        return <ListRow key={pane.key} glyph={glyph.glyph} glyphColor={glyph.color} title={pane.title} second={paneDetail(pane)} onPress={pane.gone ? undefined : () => openPane(navigation, pane)} />;
      })}
      <T dim style={{ paddingHorizontal: 12, paddingTop: 12 }}>On a computer these panes sit side by side; here they open one at a time.</T>
    </ScrollView>
  </Screen>;
}
