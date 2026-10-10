import { useCallback, useEffect, useRef, useState } from 'react';
import { ActivityIndicator, RefreshControl, View, type FlatList, type ScrollView, type SectionList } from 'react-native';
import { useFocusEffect, useIsFocused } from '@react-navigation/native';
import { offlinePresentation } from './projectionCache';
import { useStore, type OnDemand } from './store';
import { theme } from './theme';
import { Banner, Button, T } from './ui';

// What every screen's content shares: the connection, problems worth reading, and pull to refresh.
// The navigation bar and tab bar above and below are native and styled only through their options.

/** `● live example-linux · person/alex`, as stui's top-right corner says it. The first line of a list. */
/** How long ago, as a person says it: 40s, 12m, 5h. */
function ageOf(ms: number): string | null {
  if (!Number.isFinite(ms)) return null;
  const seconds = Math.max(0, Math.round(ms / 1000));
  return seconds < 60 ? `${seconds}s` : seconds < 3600 ? `${Math.floor(seconds / 60)}m` : `${Math.floor(seconds / 3600)}h`;
}

export function StatusLine() {
  const { status, hasSynced, gatewayHost, caps, snapshot, data } = useStore();
  // Not live, what is shown is st's last snapshot: say how old it is.
  const age = snapshot ? ageOf(Date.now() - Date.parse(snapshot.created_at)) : null;
  const shown = age ? `showing data from ${age} ago` : 'showing last data';
  const [glyph, color, word] = status === 'online' ? ['●', theme.idle, 'live']
    : status === 'connecting' ? ['◌', theme.waiting, hasSynced ? `updating · ${shown}` : 'connecting']
    : status === 'offline' ? ['✕', theme.fault, hasSynced ? `offline · ${shown}` : 'offline']
    : ['○', theme.quiet, 'not paired'];
  const fresh = hasSynced && !data.attention.length && !data.agents.some(agent => agent.state === 'running' && agent.harness_state === 'working');
  return <View accessibilityLabel={`${word}${gatewayHost ? ` ${gatewayHost}` : ''}`} style={{ paddingHorizontal: 12, paddingTop: 6 }}>
    <View style={{ flexDirection: 'row' }}>
    <T color={color}>{glyph} {word}</T>
    {gatewayHost ? <T dim numberOfLines={1} style={{ flex: 1 }}>  {gatewayHost}{caps?.session_actor ? ` · ${caps.session_actor.replace(/^person\//, '')}` : ''}</T> : null}
    </View>
    {fresh ? <T dim>nothing running yet · Home → New → New terminal</T> : null}
  </View>;
}

/** Errors, load failures, and the offline state, each once, at the top of the content. */
export function Banners() {
  const { error, setError, pairingIssue, setPairingIssue, loadErrors, status, hasSynced, connectionIssue, busy, actions } = useStore();
  const failed = Object.entries(loadErrors);
  return <View>
    {error ? <Banner text={error} onPress={() => setError('')} /> : null}
    {pairingIssue ? <Banner text={pairingIssue} onPress={() => setPairingIssue('')} /> : null}
    {status === 'offline' ? <View style={{ backgroundColor: theme.mantle, paddingHorizontal: 12, paddingBottom: 8 }}>
      <Banner tone="warning" text={`${offlinePresentation(hasSynced).title}${connectionIssue ? ` · reconnecting on its own: ${connectionIssue}` : ''}`} />
      <Button label="reconnect now" onPress={() => actions.reconnect()} />
    </View> : null}
    {failed.length && status === 'online' ? <Banner tone="warning" text={`not loaded: ${failed.map(([key, message]) => `${key} (${message})`).join(' · ')}`} /> : null}
    {busy ? <ActivityIndicator color={theme.accent} style={{ paddingVertical: 4 }} /> : null}
  </View>;
}

/** Load a screen's on-demand lists when it gains focus while connected. */
export function useListsOnFocus(keys: readonly OnDemand[]) {
  const { status, loadLists } = useStore();
  const joined = keys.join(',');
  useFocusEffect(useCallback(() => {
    if (status === 'online') void loadLists(joined ? joined.split(',') as OnDemand[] : []);
  }, [status, loadLists, joined]));
}

/** A screen that shows missions (a list, a card, or names of missions) asks for their window while it
 * is in front: it is large, so it is not followed otherwise. */
export function useMissionsOnFocus() {
  const { feed } = useStore();
  useFocusEffect(useCallback(() => feed?.watchMissions(), [feed]));
}

/**
 * Native pull to refresh (UIRefreshControl). The windows are live already, so a pull reads the
 * screen's on-demand lists again, or opens a fresh socket when offline.
 */
export function useRefresh(keys: readonly OnDemand[] = []) {
  const { status, loadLists, actions } = useStore();
  const [refreshing, setRefreshing] = useState(false);
  const onRefresh = useCallback(() => {
    setRefreshing(true);
    if (status !== 'online') actions.reconnect();
    void loadLists(keys).finally(() => setRefreshing(false));
  }, [status, loadLists, actions, keys.join(',')]); // eslint-disable-line react-hooks/exhaustive-deps
  return <RefreshControl refreshing={refreshing} onRefresh={onRefresh} tintColor={theme.overlay1} />;
}

type Scrollable = FlatList<unknown> | SectionList<unknown, unknown> | ScrollView | null;
/** Honour a Debug `scroll?y=` link on whichever screen is visible. */
export function useDebugScroll(ref: React.RefObject<Scrollable>) {
  const { scrollRequest } = useStore();
  const focused = useIsFocused();
  const handled = useRef(0);
  useEffect(() => {
    if (!__DEV__ || !focused || !scrollRequest || scrollRequest.at === handled.current) return;
    handled.current = scrollRequest.at;
    const list = ref.current as unknown as { scrollToOffset?: (options: { offset: number; animated: boolean }) => void; scrollTo?: (options: { y: number; animated: boolean }) => void; getScrollResponder?: () => { scrollTo?: (options: { y: number; animated: boolean }) => void } } | null;
    if (list?.scrollToOffset) list.scrollToOffset({ offset: scrollRequest.y, animated: false });
    else if (list?.scrollTo) list.scrollTo({ y: scrollRequest.y, animated: false });
    else list?.getScrollResponder?.()?.scrollTo?.({ y: scrollRequest.y, animated: false });
  }, [focused, scrollRequest, ref]);
}

export function Empty({ text }: { text: string }) {
  return <View style={{ padding: 16 }}><T dim>{text}</T></View>;
}
