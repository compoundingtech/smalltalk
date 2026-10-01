import { useMemo, useState } from 'react';
import { ScrollView, View } from 'react-native';
import { useNavigation } from '@react-navigation/native';
import type { NativeStackNavigationProp } from '@react-navigation/native-stack';
import SegmentedControl from '@react-native-segmented-control/segmented-control';
import { Banners, Empty, StatusLine, useListsOnFocus } from '../chrome';
import { glassChoices, glassGroups, type GlassLists, type PaneTarget } from '../glassesView';
import { navigationRef, type RootParams } from '../navigation';
import { useStore } from '../store';
import { theme } from '../theme';
import { ListRow, Note, Screen, SectionHeader, T } from '../ui';

// Glasses on the phone (an experiment, on in Fleet): one thing at a time. The root lists the
// chosen glass's tabs, Home first, split by split as stui shows them; a tab opens its pane.

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
  const splits = shown ? glassGroups(shown, lists) : [];
  const many = splits.length > 1;
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ paddingBottom: 32 }}>
      <StatusLine />
      {!glassesGranted ? <Note tone="warning">This gateway does not keep spaces yet, or this device was paired before it could. Spaces made in stui --spaces appear here once it does.</Note> : null}
      {glassesIssue ? <Note tone="fault">{glassesIssue}</Note> : null}
      {choices.length > 1 ? <View style={{ paddingHorizontal: 12, paddingVertical: 8 }}>
        <SegmentedControl
          values={choices.map(choice => choice.name)}
          selectedIndex={Math.max(0, choices.findIndex(choice => choice.id === shownId))}
          onChange={event => setChosen(choices[event.nativeEvent.selectedSegmentIndex]?.id ?? null)}
        />
      </View> : null}
      {shown ? <>
        <SectionHeader title={shown.body?.name ?? 'space'} count={splits.reduce((sum, split) => sum + split.tabs.length, 1)} />
        <ListRow glyph="⌂" glyphColor={theme.accent} title="Home" second="what needs you" onPress={() => navigationRef.navigate('Home', { screen: 'HomeRoot' })} />
        {splits.map(split => <View key={split.index}>
          {many ? <SectionHeader title={`split ${split.index + 1} of ${splits.length}`} count={split.tabs.length} /> : null}
          {split.tabs.map(tab => {
            const glyph = paneGlyph(tab.pane);
            return <ListRow
              key={`${tab.group}-${tab.index}`}
              glyph={glyph.glyph}
              glyphColor={glyph.color}
              title={tab.title}
              second={paneDetail(tab.pane)}
              onPress={tab.pane.gone ? undefined : () => openPane(navigation, tab.pane)}
            />;
          })}
          {many && split.tabs.length === 0 ? <T dim style={{ paddingHorizontal: 12, paddingVertical: 6 }}>Empty on the computer too.</T> : null}
        </View>)}
      </> : glassesGranted ? <Empty text="No spaces yet. Make one with stui --spaces on a computer." /> : null}
    </ScrollView>
  </Screen>;
}
