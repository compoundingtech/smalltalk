import { useEffect, useState } from 'react';
import { Linking, StatusBar, View } from 'react-native';
import { DarkTheme, NavigationContainer, getFocusedRouteNameFromRoute, type RouteProp } from '@react-navigation/native';
import { createNativeStackNavigator, type NativeStackNavigationOptions } from '@react-navigation/native-stack';
import { createNativeBottomTabNavigator } from '@react-navigation/bottom-tabs/unstable';
import { SafeAreaProvider } from 'react-native-safe-area-context';
import { useFonts } from 'expo-font';
// Only the four weights the content uses, not the whole family.
import { IBMPlexMono_400Regular } from '@expo-google-fonts/ibm-plex-mono/400Regular';
import { IBMPlexMono_400Regular_Italic } from '@expo-google-fonts/ibm-plex-mono/400Regular_Italic';
import { IBMPlexMono_600SemiBold } from '@expo-google-fonts/ibm-plex-mono/600SemiBold';
import { IBMPlexMono_700Bold } from '@expo-google-fonts/ibm-plex-mono/700Bold';
import { homeRows } from './homeView';
import { FULL_SCREEN, navigationRef, ROOTS, type StackParams, type TabParams } from './navigation';
import { StoreProvider, useStore } from './store';
import { parseDevLink, tabNamed, type Tab } from './tabs';
import { theme } from './theme';
import { AgentsScreen, HistoryScreen } from './screens/Agents';
import { ConversationScreen } from './screens/Conversation';
import { FleetScreen, PairScreen } from './screens/Fleet';
import { UsageDetailScreen, UsageScreen } from './screens/Usage';
import { NewAgentScreen } from './screens/NewAgent';
import { AttentionScreen, HomeScreen } from './screens/Home';
import { LaunchScreen, MissionScreen, MissionsScreen, NewMissionScreen } from './screens/Missions';
import { SelectTextScreen } from './screens/SelectText';
import { TerminalScreen } from './screens/Terminal';
import { GlassesScreen, SpaceScreen } from './screens/Glasses';
import { FabricProofScreen } from './screens/FabricProof';
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

const ICONS: Record<Tab, string> = { Home: 'house', Agents: 'person.2', Missions: 'point.3.connected.trianglepath.dotted', Fleet: 'server.rack' };
const ROOT_SCREENS = { HomeRoot: HomeScreen, AgentsRoot: AgentsScreen, MissionsRoot: MissionsScreen, FleetRoot: FleetScreen } as const;

// Native header options only: the system font and look, tinted with the accent.
const stackOptions: NativeStackNavigationOptions = {
  headerTintColor: theme.accent,
  headerStyle: { backgroundColor: theme.mantle },
  headerTitleStyle: { color: theme.text },
  headerLargeTitleStyle: { color: theme.text },
  contentStyle: { backgroundColor: theme.base },
  headerBackButtonDisplayMode: 'default',
};

function TabStack({ tab }: { tab: Tab | 'Glasses' }) {
  return <Stack.Navigator screenOptions={stackOptions}>
    {tab === 'Glasses'
      ? <Stack.Screen name="GlassesRoot" component={GlassesScreen} options={{ title: 'Spaces' }} />
      : <Stack.Screen name={ROOTS[tab] as keyof typeof ROOT_SCREENS} component={ROOT_SCREENS[ROOTS[tab] as keyof typeof ROOT_SCREENS]} options={{ title: tab }} />}
    <Stack.Screen name="Space" component={SpaceScreen} options={{ title: 'Space' }} />
    <Stack.Screen name="Conversation" component={ConversationScreen} options={{ title: 'Conversation' }} />
    <Stack.Screen name="SelectText" component={SelectTextScreen} options={{ title: 'Select text', presentation: 'formSheet', sheetAllowedDetents: [0.6, 1], sheetGrabberVisible: true }} />
    <Stack.Screen name="Terminal" component={TerminalScreen} options={{ title: 'Terminal', contentStyle: { backgroundColor: theme.crust } }} />
    <Stack.Screen name="Mission" component={MissionScreen} options={{ title: 'Mission' }} />
    <Stack.Screen name="Attention" component={AttentionScreen} options={{ title: 'Needs you' }} />
    <Stack.Screen name="Launch" component={LaunchScreen} options={{ title: 'Launch' }} />
    <Stack.Screen name="History" component={HistoryScreen} options={{ title: 'Past sessions' }} />
    <Stack.Screen name="NewMission" component={NewMissionScreen} options={{ title: 'New mission', presentation: 'modal' }} />
    <Stack.Screen name="NewAgent" component={NewAgentScreen} options={{ title: 'New agent', presentation: 'modal' }} />
    <Stack.Screen name="Usage" component={UsageScreen} options={{ title: 'Usage' }} />
    <Stack.Screen name="UsageDetail" component={UsageDetailScreen} options={{ title: 'Usage' }} />
  </Stack.Navigator>;
}
const TAB_COMPONENTS: Record<Tab, () => React.JSX.Element> = {
  Home: () => <TabStack tab="Home" />,
  Agents: () => <TabStack tab="Agents" />,
  Missions: () => <TabStack tab="Missions" />,
  Fleet: () => <TabStack tab="Fleet" />,
};
const GlassesTab = () => <TabStack tab="Glasses" />;

function tabBarHidden(route: RouteProp<TabParams>): boolean {
  const focused = getFocusedRouteNameFromRoute(route) as keyof StackParams | undefined;
  return !!focused && FULL_SCREEN.includes(focused);
}

function Main() {
  const [fabricProof, setFabricProof] = useState<FabricProofInput | null>(null);
  const { credential, url, order, data, caps, actions, setTreeView, requestScroll, glassesOn } = useStore();
  const homeCount = homeRows(data.attention, caps?.session_actor).length;
  const paired = !!url && !!credential;

  // Debug-only deep links for simulator checks; disabled in Release.
  useEffect(() => {
    if (!__DEV__) return;
    let lastPair = '';
    const handle = (link: string | null) => {
      if (!link) return;
      const proof = parseFabricProofLink(link);
      if (proof) { setFabricProof(proof); return; }
      const parsed = parseDevLink(link);
      if (!parsed) return;
      if (parsed.kind === 'pair') { if (lastPair !== link) { lastPair = link; void actions.pairFromLink(parsed.gateway, parsed.id, parsed.code); } return; }
      if (parsed.kind === 'tree') { setTreeView(parsed.on); if (navigationRef.isReady()) navigationRef.navigate('Agents', { screen: 'AgentsRoot' }); return; }
      if (parsed.kind === 'scroll') { requestScroll(parsed.y); return; }
      if (!navigationRef.isReady()) { setTimeout(() => handle(link), 300); return; }
      if (parsed.kind === 'tab') navigationRef.navigate(parsed.tab, { screen: ROOTS[parsed.tab] } as never);
      else if (parsed.kind === 'mission') navigationRef.navigate('Missions', { screen: 'Mission', params: { id: parsed.id }, initial: false });
      else if (parsed.kind === 'agent') navigationRef.navigate('Agents', { screen: 'Conversation', params: { target: parsed.id }, initial: false });
      else if (parsed.kind === 'terminal') navigationRef.navigate('Agents', { screen: 'Terminal', params: { terminalId: parsed.id }, initial: false });
      else if (parsed.kind === 'session') {
        navigationRef.navigate('Agents', { screen: 'Conversation', params: { target: parsed.id, sessionId: parsed.id }, initial: false });
        if (parsed.terminal) setTimeout(() => navigationRef.navigate('Agents', { screen: 'Terminal', params: { terminalId: parsed.terminal! }, initial: false }), 300);
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

  if (__DEV__ && fabricProof) return <FabricProofScreen input={fabricProof} onClose={() => setFabricProof(null)} />;
  if (!paired) {
    return <Stack.Navigator screenOptions={stackOptions}>
      <Stack.Screen name="HomeRoot" component={PairScreen} options={{ title: 'Pair this device' }} />
    </Stack.Navigator>;
  }
  const initial = (__DEV__ ? tabNamed(process.env.EXPO_PUBLIC_ST3_TEST_TAB) : null) ?? order[0];
  return <Tabs.Navigator
    initialRouteName={initial}
    screenOptions={({ route }) => ({
      headerShown: false,
      tabBarActiveTintColor: theme.accent,
      // Spaces: one rounded rect, as stui's ▢.
      tabBarIcon: { type: 'sfSymbol', name: (route.name === 'Glasses' ? 'app' : ICONS[route.name as Tab]) as never },
      tabBarStyle: { display: tabBarHidden(route) ? 'none' : 'flex' },
    })}
  >
    {order.map(tab => <Tabs.Screen
      key={tab}
      name={tab}
      component={TAB_COMPONENTS[tab]}
      options={tab === 'Home' && homeCount ? { tabBarBadge: homeCount, tabBarBadgeStyle: { backgroundColor: theme.person, color: theme.crust } } : {}}
    />)}
    {glassesOn ? <Tabs.Screen name="Glasses" component={GlassesTab} options={{ title: 'Spaces', tabBarLabel: 'Spaces' }} /> : null}
  </Tabs.Navigator>;
}

export default function App() {
  const [fontsLoaded] = useFonts({ IBMPlexMono_400Regular, IBMPlexMono_400Regular_Italic, IBMPlexMono_600SemiBold, IBMPlexMono_700Bold });
  if (!fontsLoaded) return <View style={{ flex: 1, backgroundColor: theme.base }} />;
  return <SafeAreaProvider>
    <StatusBar barStyle="light-content" />
    <StoreProvider>
      <NavigationContainer ref={navigationRef} theme={navigationTheme}>
        <Main />
      </NavigationContainer>
    </StoreProvider>
  </SafeAreaProvider>;
}
