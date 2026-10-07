import { useEffect, useLayoutEffect, useState } from 'react';
import { ScrollView } from 'react-native';
import type { RootScreen } from '../navigation';
import { useStore } from '../store';
import { Banners, Empty } from '../chrome';
import { Markdown, Screen, T } from '../ui';
import { theme } from '../theme';
import { documentTitle } from '../documents';

// An st document opened from a card or a message, read from st and drawn as markdown, so a
// proposal can be read where it is asked about (Nathan, 2026-10-07: "I can't read this document").
export function DocumentScreen({ route, navigation }: RootScreen<'Document'>) {
  const { name } = route.params;
  const { actions, status } = useStore();
  const [state, setState] = useState<{ text: string } | { error: string } | null>(null);
  useLayoutEffect(() => { navigation.setOptions({ title: documentTitle(name) }); }, [navigation, name]);
  useEffect(() => {
    if (status !== 'online') return;
    let current = true;
    actions.document(name).then(text => { if (current) setState({ text }); }, error => { if (current) setState({ error: String(error?.message ?? error) }); });
    return () => { current = false; };
  }, [name, status]);
  if (!state) return <Screen><Banners /><Empty text={status === 'online' ? 'Reading the document…' : 'Offline: the document is read when st is reachable.'} /></Screen>;
  if ('error' in state) return <Screen><Banners /><Empty text={`Could not read it: ${state.error}`} /></Screen>;
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ padding: 12, gap: 8, paddingBottom: 32 }}>
      <Markdown text={state.text} color={theme.text} />
      <T dim selectable>{name}</T>
    </ScrollView>
  </Screen>;
}
