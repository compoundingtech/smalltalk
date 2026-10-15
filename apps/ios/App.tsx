import { useEffect, useLayoutEffect, useState } from 'react';
import { Linking, View, useColorScheme } from 'react-native';
import { DarkTheme, DefaultTheme, ThemeProvider, NavigationContainer, getFocusedRouteNameFromRoute, type RouteProp } from '@react-navigation/native';
import { createNativeStackNavigator, type NativeStackNavigationOptions } from '@react-navigation/native-stack';
import { createNativeBottomTabNavigator } from '@react-navigation/bottom-tabs/unstable';
import { SafeAreaProvider } from 'react-native-safe-area-context';
import { useFonts } from 'expo-font';
// Only the four weights the content uses, not the whole family.
import { IBMPlexMono_400Regular } from '@expo-google-fonts/ibm-plex-mono/400Regular';
import { IBMPlexMono_400Regular_Italic } from '@expo-google-fonts/ibm-plex-mono/400Regular_Italic';
import { IBMPlexMono_600SemiBold } from '@expo-google-fonts/ibm-plex-mono/600SemiBold';
import { IBMPlexMono_700Bold } from '@expo-google-fonts/ibm-plex-mono/700Bold';
import { openAlerts } from '@smalltalk/st3-views';
import { FULL_SCREEN, navigationRef, ROOTS, type StackParams, type TabParams } from './navigation';
import { StoreProvider, useStore } from './store';
import { parseDevLink, tabLabel, tabNamed, type Tab } from './tabs';
import { systemColors, theme } from './theme';
import { AgentsScreen, HistoryScreen } from './screens/Agents';
import { ConversationScreen } from './screens/Conversation';
import { FleetScreen, PairScreen } from './screens/Fleet';
import { UsageDetailScreen, UsageScreen } from './screens/Usage';
import { AttentionScreen, NowScreen } from './screens/Now';
import { LaunchScreen, MissionScreen, MissionsScreen, NewMissionScreen } from './screens/Missions';
import { TerminalScreen } from './screens/Terminal';
import { GlassesScreen, SpaceScreen } from './screens/Glasses';
import { ResourceScreen } from './screens/Resource';
import { SidebarScreen } from './screens/Sidebar';
import { FabricProofScreen } from './screens/FabricProof';
import { quality } from './qualityNative';
import { qualityVisibility } from './qualityView';
import { parseFabricProofLink, type FabricProofInput } from './fabricProof';

// The chrome is native: one UITabBarController (react-native-screens' tabs, through
// @react-navigation/bottom-tabs' native navigator) holding a UINavigationController per tab
// (@react-navigation/native-stack). It keeps the system look and fonts. Everything inside a
// screen is React, drawn like stui: IBM Plex Mono on the Mocha palette.

const Tabs = createNativeBottomTabNavigator<TabParams>();
const Stack = createNativeStackNavigator<StackParams>();

const navigationTheme = {
  ...DarkTheme,
  colors: { ...DarkTheme.colors, primary: theme.accent, background: theme.base, card: theme.mantle, text: theme.text, border: theme.surface0, notification: theme.person },
};

const ICONS: Record<Tab, string> = { Now: 'bell', Resources: 'folder', Fleet: 'server.rack', Spaces: 'app' };

// Native header options only: the system font and look, tinted with the accent.
const stackOptions: NativeStackNavigationOptions = {
  headerTintColor: theme.accent,
  headerStyle: { backgroundColor: theme.mantle },
  headerTitleStyle: { color: theme.text },
  headerLargeTitleStyle: { color: theme.text },
  contentStyle: { backgroundColor: theme.base },
  statusBarStyle: 'light',
  headerBackButtonDisplayMode: 'default',
};

function TabStack({ tab }: { tab: Tab }) {
  const scheme = useColorScheme() === 'dark' ? 'dark' : 'light';
  const colors = systemColors[scheme];
  const systemOptions: NativeStackNavigationOptions = {
    headerTintColor: colors.action,
    headerStyle: { backgroundColor: colors.background },
    headerTitleStyle: { color: colors.text },
    headerLargeTitleStyle: { color: colors.text },
    contentStyle: { backgroundColor: colors.background },
    statusBarStyle: scheme === 'dark' ? 'light' : 'dark',
  };
  return <Stack.Navigator screenOptions={stackOptions}>
    {tab === 'Now' ? <Stack.Screen name="NowRoot" component={NowScreen} options={{ ...systemOptions, title: 'Now' }} /> :
     tab === 'Resources' ? <Stack.Screen name="ResourcesRoot" component={SidebarScreen} options={{ title: 'Resources' }} /> :
     tab === 'Fleet' ? <Stack.Screen name="FleetRoot" component={FleetScreen} options={{ title: 'Fleet' }} /> :
     <Stack.Screen name="SpacesRoot" component={GlassesScreen} options={{ title: 'Spaces' }} />}

    <Stack.Screen name="Agents" component={AgentsScreen} options={{ title: 'Agents' }} />
    <Stack.Screen name="Missions" component={MissionsScreen} options={{ title: 'Missions' }} />
    <Stack.Screen name="Resource" component={ResourceScreen} options={{ title: 'Resource' }} />
    <Stack.Screen name="Space" component={SpaceScreen} options={{ title: 'Space' }} />
    <Stack.Screen name="Conversation" component={ConversationScreen} options={{ title: 'Conversation' }} />
    <Stack.Screen name="Terminal" component={TerminalScreen} options={{ title: 'Terminal', contentStyle: { backgroundColor: theme.crust } }} />
    <Stack.Screen name="Mission" component={MissionScreen} options={{ title: 'Mission' }} />
    <Stack.Screen name="Attention" component={AttentionScreen} options={{ ...systemOptions, title: 'Alert' }} />
    <Stack.Screen name="Launch" component={LaunchScreen} options={{ title: 'Launch' }} />
    <Stack.Screen name="History" component={HistoryScreen} options={{ title: 'Past sessions' }} />
    <Stack.Screen name="NewMission" component={NewMissionScreen} options={{ title: 'New mission', presentation: 'modal' }} />
    <Stack.Screen name="Usage" component={UsageScreen} options={{ title: 'Usage' }} />
    <Stack.Screen name="UsageDetail" component={UsageDetailScreen} options={{ title: 'Usage' }} />
  </Stack.Navigator>;
}
const TAB_COMPONENTS: Record<Tab, () => React.JSX.Element> = {
  Now: () => <TabStack tab="Now" />,
  Resources: () => <TabStack tab="Resources" />,
  Fleet: () => <TabStack tab="Fleet" />,
  Spaces: () => <TabStack tab="Spaces" />,
};

function tabBarHidden(route: RouteProp<TabParams>): boolean {
  const focused = getFocusedRouteNameFromRoute(route) as keyof StackParams | undefined;
  return !!focused && FULL_SCREEN.includes(focused);
}

function Main({ proofSession = false }: { proofSession?: boolean }) {
  const { pairDraft, credential, url, order, data, caps, actions, requestScroll, status } = useStore();
  const [shownRoute, setShownRoute] = useState(() => navigationRef.getCurrentRoute());
  useEffect(() => {
    setShownRoute(navigationRef.getCurrentRoute());
    return navigationRef.addListener('state', () => setShownRoute(navigationRef.getCurrentRoute()));
  }, []);
  const homeCount = openAlerts(data.attention, caps?.session_actor).length;
  const paired = !!url && !!credential;
  const shownName = shownRoute?.name ?? ROOTS[order[0]];
  const scheme = useColorScheme() === 'dark' ? 'dark' : 'light';
  const colors = systemColors[scheme];
  const target = (shownRoute?.params as { target?: string } | undefined)?.target;
  const shownMask = qualityVisibility(shownName, paired, target, data.agents, data.sessions);
  useLayoutEffect(() => {
    if (!proofSession) quality.commit(shownMask, paired && quality.windowsReady && status === 'online', status === 'online' ? 2 : status === 'offline' ? 3 : status === 'connecting' ? 1 : 0, quality.loadingMask);
  }, [proofSession, shownMask, paired, status, data.attention, data.agents]);

  // Debug-only deep links for simulator checks; disabled in Release.
  useEffect(() => {
    if (!__DEV__) return;
    let lastPair = '';
    const handle = (link: string | null) => {
      if (!link) return;
      const parsed = parseDevLink(link);
      if (!parsed) return;
      if (parsed.kind === 'pair' && proofSession) return;
      if (parsed.kind === 'pair') { if (lastPair !== link) { lastPair = link; void actions.pairFromLink(parsed.gateway, parsed.id, parsed.code); } return; }
      if (parsed.kind === 'scroll') { requestScroll(parsed.y); return; }
      if (!navigationRef.isReady()) { setTimeout(() => handle(link), 300); return; }
      if (parsed.kind === 'tab') navigationRef.navigate(parsed.tab, { screen: ROOTS[parsed.tab] } as never);
      else if (parsed.kind === 'attention') navigationRef.navigate('Now', { screen: 'Attention', params: { id: parsed.id }, initial: false });
      else if (parsed.kind === 'mission') navigationRef.navigate('Resources', { screen: 'Mission', params: { id: parsed.id }, initial: false });
      else if (parsed.kind === 'agent') navigationRef.navigate('Resources', { screen: 'Conversation', params: { target: parsed.id }, initial: false });
      else if (parsed.kind === 'terminal') navigationRef.navigate('Resources', { screen: 'Terminal', params: { terminalId: parsed.id }, initial: false });
      else if (parsed.kind === 'session') {
        navigationRef.navigate('Resources', { screen: 'Conversation', params: { target: parsed.id, sessionId: parsed.id }, initial: false });
        if (parsed.terminal) setTimeout(() => navigationRef.navigate('Resources', { screen: 'Terminal', params: { terminalId: parsed.terminal! }, initial: false }), 300);
      }
    };
    void Linking.getInitialURL().then(handle);
    handle(process.env.EXPO_PUBLIC_ST3_FABRIC_PROOF_LINK ?? null);
    handle(process.env.EXPO_PUBLIC_ST3_TEST_PAIR_LINK ?? null);
    // Links to follow at launch, six seconds apart, for headless screenshots: the simulator asks
    // before opening each link it is handed, and nobody is there to answer.
    (process.env.EXPO_PUBLIC_ST3_TEST_LINKS ?? '').split(/\s+/).filter(Boolean).forEach((link: string, index: number) => setTimeout(() => handle(link), 6000 * (index + 1)));
    const subscription = Linking.addEventListener('url', event => handle(event.url));
    return () => subscription.remove();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  if (!paired || pairDraft) {
    return <Stack.Navigator screenOptions={stackOptions}>
      <Stack.Screen name="NowRoot" component={PairScreen} options={{ title: 'Pair this device' }} />
    </Stack.Navigator>;
  }
  const initial = (__DEV__ ? tabNamed(process.env.EXPO_PUBLIC_ST3_TEST_TAB) : null) ?? order[0];
  const systemSurface = shownName === 'NowRoot' || shownName === 'Attention';
  return <ThemeProvider value={systemSurface ? {
    ...(scheme === 'dark' ? DarkTheme : DefaultTheme),
    colors: { ...(scheme === 'dark' ? DarkTheme.colors : DefaultTheme.colors), primary: colors.action, background: colors.background, card: colors.grouped, text: colors.text, border: colors.separator, notification: colors.attention },
  } : navigationTheme}><Tabs.Navigator
    initialRouteName={initial}
    screenOptions={({ route }) => ({
      headerShown: false,
      tabBarActiveTintColor: systemSurface ? colors.action : theme.accent,
      // Spaces: one rounded rect, as stui's ▢.
      tabBarIcon: { type: 'sfSymbol', name: ICONS[route.name as Tab] as never },
      tabBarStyle: { display: tabBarHidden(route) ? 'none' : 'flex' },
    })}
  >
    {order.map(tab => <Tabs.Screen
      key={tab}
      name={tab}
      component={TAB_COMPONENTS[tab]}
      options={{ tabBarLabel: tabLabel(tab), title: tabLabel(tab), ...(tab === 'Now' && homeCount ? { tabBarBadge: homeCount, tabBarBadgeStyle: { backgroundColor: systemSurface ? colors.attention : theme.person, color: systemSurface ? colors.background : theme.crust } } : {}) }}
    />)}
  </Tabs.Navigator></ThemeProvider>;
}

const qualityLayout = () => quality.commit();

export default function App() {
  const [fabricProof, setFabricProof] = useState<FabricProofInput | null>(null);
  useEffect(() => {
    if (!__DEV__) return;
    const handle = (link: string | null) => { if (link) { const input = parseFabricProofLink(link); if (input) setFabricProof(current =>
      // Repeated OS delivery cannot consume a one-time pairing twice. A new mode
      // or trial uses a fresh pairing link, whose challenge id changes this key.
      current?.node === input.node && current.service === input.service && current.id === input.id ? current : input); } };
    void Linking.getInitialURL().then(handle);
    handle(process.env.EXPO_PUBLIC_ST3_FABRIC_PROOF_LINK ?? null);
    const subscription = Linking.addEventListener('url', event => handle(event.url));
    return () => subscription.remove();
  }, []);
  const [fontsLoaded] = useFonts({ IBMPlexMono_400Regular, IBMPlexMono_400Regular_Italic, IBMPlexMono_600SemiBold, IBMPlexMono_700Bold });
  if (!fontsLoaded) return <View onLayout={qualityLayout} style={{ flex: 1, backgroundColor: theme.base }} />;
  return <SafeAreaProvider onLayout={qualityLayout}>
    {__DEV__ && fabricProof ? <FabricProofScreen key={fabricProof.id ?? `${fabricProof.node}/${fabricProof.service}`} input={fabricProof} onClose={() => setFabricProof(null)} renderClient={proof =>
      <StoreProvider proof={proof}>
        <NavigationContainer ref={navigationRef} theme={navigationTheme}><Main proofSession /></NavigationContainer>
      </StoreProvider>
    } /> : <StoreProvider>
      <NavigationContainer ref={navigationRef} theme={navigationTheme}><Main /></NavigationContainer>
    </StoreProvider>}
  </SafeAreaProvider>;
}
