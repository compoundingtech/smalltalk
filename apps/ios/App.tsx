import { useCallback, useEffect, useMemo, useState } from 'react';
import { Linking, ScrollView, StyleSheet, Text, TextInput, View } from 'react-native';
import AsyncStorage from '@react-native-async-storage/async-storage';
import * as SecureStore from 'expo-secure-store';
import * as Crypto from 'expo-crypto';
import { DarkTheme, NavigationContainer, type LinkingOptions, type ParamListBase, type Theme } from '@react-navigation/native';
import { createBottomTabNavigator } from '@react-navigation/bottom-tabs';
import { createNativeStackNavigator, type NativeStackNavigationOptions } from '@react-navigation/native-stack';
import { SafeAreaProvider, SafeAreaView } from 'react-native-safe-area-context';
import { API_VERSION, St3Client } from '../../clients/typescript/st3-client';
import { useDemoStore } from './demoStore';
import { errorText, useLiveStore } from './liveStore';
import { gatewayTransport, LAN_HTTP_WARNING, normalizeGatewayUrl } from './gatewayUrl';
import { PROJECTION_CACHE_KEY } from './projectionCache';
import { homeBadge } from './screenModel';
import { AgentDetailsScreen, AgentScreen, AgentsScreen, AttentionScreen, DeclarationScreen, FleetScreen, HeaderRight, HomeScreen, MachineScreen, MissionScreen, MissionsScreen, PeekScreen, SettingsScreen, WorktreeScreen, WorktreesScreen } from './screens';
import { StoreContext, useStore, type Store } from './store';
import { colors } from './theme';
import { Body, Button, Buttons, Dim, styles as ui } from './ui';

const URL_KEY = 'st3.gateway.url', CREDENTIAL_KEY = 'st3.device.credential', DEMO_KEY = 'st.demo';
const SCHEME = 'com.compoundingtech.smalltalk.starter://';

const navigationTheme: Theme = {
  ...DarkTheme,
  colors: { ...DarkTheme.colors, primary: colors.accent, background: colors.base, card: colors.mantle, text: colors.text, border: colors.surface0, notification: colors.person },
};

const Tabs = createBottomTabNavigator();
const Stack = createNativeStackNavigator();

const stackOptions: NativeStackNavigationOptions = {
  headerStyle: { backgroundColor: colors.base },
  headerTintColor: colors.accent,
  headerTitleStyle: { color: colors.text },
  headerLargeTitleStyle: { color: colors.text },
  headerShadowVisible: false,
  contentStyle: { backgroundColor: colors.base },
};
const listOptions = (title: string): NativeStackNavigationOptions => ({ title, headerLargeTitle: true, headerRight: () => <HeaderRight /> });

const HomeStack = () => (
  <Stack.Navigator initialRouteName="Home" screenOptions={stackOptions}>
    <Stack.Screen name="Home" component={HomeScreen} options={listOptions('What needs you')} />
    <Stack.Screen name="Attention" component={AttentionScreen} options={{ title: '' }} />
  </Stack.Navigator>
);
const AgentsStack = () => (
  <Stack.Navigator initialRouteName="Agents" screenOptions={stackOptions}>
    <Stack.Screen name="Agents" component={AgentsScreen} options={listOptions('Agents')} />
    <Stack.Screen name="Agent" component={AgentScreen} options={{ title: '' }} />
  </Stack.Navigator>
);
const MissionsStack = () => (
  <Stack.Navigator initialRouteName="Missions" screenOptions={stackOptions}>
    <Stack.Screen name="Missions" component={MissionsScreen} options={listOptions('Missions')} />
    <Stack.Screen name="Mission" component={MissionScreen} options={{ title: '' }} />
    <Stack.Screen name="Declaration" component={DeclarationScreen} options={{ title: 'Declaration' }} />
  </Stack.Navigator>
);
const FleetStack = () => (
  <Stack.Navigator initialRouteName="Fleet" screenOptions={stackOptions}>
    <Stack.Screen name="Fleet" component={FleetScreen} options={listOptions('Fleet')} />
    <Stack.Screen name="Machine" component={MachineScreen} options={{ title: '' }} />
  </Stack.Navigator>
);
const WorktreesStack = () => (
  <Stack.Navigator initialRouteName="Worktrees" screenOptions={stackOptions}>
    <Stack.Screen name="Worktrees" component={WorktreesScreen} options={listOptions('Worktrees')} />
    <Stack.Screen name="Worktree" component={WorktreeScreen} options={{ title: 'Worktree' }} />
  </Stack.Navigator>
);

const tabIcon = (glyph: string) => ({ color }: { color: string }) => <Text style={{ color, fontSize: 18, fontWeight: '700' }}>{glyph}</Text>;

function TabsScreen() {
  const { world } = useStore();
  const badge = homeBadge(world);
  return (
    <Tabs.Navigator screenOptions={{ headerShown: false, tabBarActiveTintColor: colors.accent, tabBarInactiveTintColor: colors.overlay1, tabBarStyle: { backgroundColor: colors.mantle, borderTopColor: colors.surface0 } }}>
      <Tabs.Screen name="HomeTab" component={HomeStack} options={{
        title: 'Home', tabBarIcon: tabIcon('◆'),
        tabBarBadge: badge.count || undefined,
        tabBarBadgeStyle: { backgroundColor: badge.stopped ? colors.person : colors.surface2, color: colors.crust, fontWeight: '800' },
      }} />
      <Tabs.Screen name="AgentsTab" component={AgentsStack} options={{ title: 'Agents', tabBarIcon: tabIcon('●') }} />
      <Tabs.Screen name="MissionsTab" component={MissionsStack} options={{ title: 'Missions', tabBarIcon: tabIcon('▰') }} />
      <Tabs.Screen name="FleetTab" component={FleetStack} options={{ title: 'Fleet', tabBarIcon: tabIcon('◉') }} />
      <Tabs.Screen name="WorktreesTab" component={WorktreesStack} options={{
        title: 'Worktrees',
        // Invented data until st has worktree resources, and the tab says so.
        tabBarIcon: ({ color }) => (
          <View style={local.demoIcon}>
            <Text style={{ color, fontSize: 18, fontWeight: '700' }}>⑂</Text>
            <View style={local.demoTag}><Text numberOfLines={1} style={local.demoTagText}>demo</Text></View>
          </View>
        ),
      }} />
    </Tabs.Navigator>
  );
}

// Navigation links carry no authorization; like the pairing link they exist for Debug smoke
// tests and screenshots, and Release builds do not register them.
const linking: LinkingOptions<ParamListBase> | undefined = __DEV__ ? {
  prefixes: [SCHEME],
  config: {
    screens: {
      Tabs: {
        screens: {
          HomeTab: { screens: { Home: 'home', Attention: 'attention' } },
          AgentsTab: { screens: { Agents: 'agents', Agent: 'agent' } },
          MissionsTab: { screens: { Missions: 'missions', Mission: 'mission', Declaration: 'declaration' } },
          FleetTab: { screens: { Fleet: 'fleet', Machine: 'machine' } },
          WorktreesTab: { screens: { Worktrees: 'worktrees', Worktree: 'worktree' } },
        },
      },
      Peek: 'peek',
      AgentDetails: 'details',
      Settings: 'settings',
    },
    // Nested screens are typed per param list; these routes are untyped by design.
  } as unknown as LinkingOptions<ParamListBase>['config'],
} : undefined;

function Navigation({ store }: { store: Store }) {
  return (
    <StoreContext.Provider value={store}>
      <NavigationContainer theme={navigationTheme} linking={linking}>
        <Stack.Navigator screenOptions={stackOptions}>
          <Stack.Screen name="Tabs" component={TabsScreen} options={{ headerShown: false }} />
          <Stack.Screen name="Peek" component={PeekScreen} options={{ presentation: 'formSheet', sheetAllowedDetents: [0.45, 0.9], sheetGrabberVisible: true, headerShown: false }} />
          <Stack.Screen name="AgentDetails" component={AgentDetailsScreen} options={{ presentation: 'formSheet', sheetAllowedDetents: [0.6, 1], sheetGrabberVisible: true, headerShown: false }} />
          <Stack.Screen name="Settings" component={SettingsScreen} options={{ presentation: 'modal', title: 'Settings' }} />
        </Stack.Navigator>
      </NavigationContainer>
    </StoreContext.Provider>
  );
}

function DemoApp({ leave }: { leave: () => Promise<void> }) {
  return <Navigation store={useDemoStore(leave)} />;
}

function LiveApp({ url, credential, leave }: { url: string; credential: string; leave: () => Promise<void> }) {
  return <Navigation store={useLiveStore({ url, credential, leave })} />;
}

type Mode = { kind: 'starting' } | { kind: 'connect' } | { kind: 'demo' } | { kind: 'live'; url: string; credential: string };

export default function App() {
  const [mode, setMode] = useState<Mode>({ kind: 'starting' });
  const [url, setUrl] = useState(''), [urlDraft, setUrlDraft] = useState('');
  const [pairingId, setPairingId] = useState(''), [pairingCode, setPairingCode] = useState('');
  const [issue, setIssue] = useState(''), [busy, setBusy] = useState(false);

  useEffect(() => {
    void Promise.allSettled([AsyncStorage.getItem(URL_KEY), SecureStore.getItemAsync(CREDENTIAL_KEY), AsyncStorage.getItem(DEMO_KEY)]).then(([u, c, d]) => {
      const savedUrl = u.status === 'fulfilled' ? u.value : null;
      if (savedUrl) { setUrl(savedUrl); setUrlDraft(savedUrl); }
      if (c.status === 'rejected') setIssue('Secure credential storage is unavailable on this build.');
      setMode(current => {
        if (current.kind !== 'starting') return current; // a launch link already chose
        if ((d.status === 'fulfilled' && d.value === '1') || (__DEV__ && process.env.EXPO_PUBLIC_ST_DEMO === '1')) return { kind: 'demo' };
        if (savedUrl && c.status === 'fulfilled' && c.value) return { kind: 'live', url: savedUrl, credential: c.value };
        return { kind: 'connect' };
      });
    });
  }, []);

  const completePairing = useCallback(async (gateway: string, id: string, code: string) => {
    setBusy(true);
    setIssue('');
    try {
      const publicKey = Array.from(Crypto.getRandomBytes(32), byte => byte.toString(16).padStart(2, '0')).join('');
      const result = await new St3Client({ baseUrl: gateway }).completePairing(id, { api_version: API_VERSION, code, device_public_key: publicKey });
      await AsyncStorage.removeItem(PROJECTION_CACHE_KEY).catch(() => {});
      await SecureStore.setItemAsync(CREDENTIAL_KEY, result.value.credential, { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY });
      await AsyncStorage.setItem(URL_KEY, gateway);
      await AsyncStorage.removeItem(DEMO_KEY);
      setUrl(gateway); setUrlDraft(gateway); setPairingId(''); setPairingCode('');
      setMode({ kind: 'live', url: gateway, credential: result.value.credential });
    } catch (error) { setIssue(`Pairing failed: ${errorText(error)}`); }
    finally { setBusy(false); }
  }, []);

  // Debug builds accept a short-lived pairing link and a demo link for headless simulator
  // checks. Treat a pairing link as a temporary credential: never commit or log it.
  useEffect(() => {
    if (!__DEV__) return;
    let handled = '';
    const handle = (link: string | null) => {
      if (!link || handled === link) return;
      let parsed: URL;
      try { parsed = new URL(link); } catch { return; }
      if (parsed.hostname === 'demo') { handled = link; void AsyncStorage.setItem(DEMO_KEY, '1'); setMode({ kind: 'demo' }); return; }
      if (parsed.hostname !== 'pair') return;
      const gateway = normalizeGatewayUrl(parsed.searchParams.get('gateway') ?? '');
      const id = parsed.searchParams.get('id'), code = parsed.searchParams.get('code');
      if (!gateway || !id || !code) return;
      handled = link;
      void completePairing(gateway, id, code);
    };
    void Linking.getInitialURL().then(handle);
    handle(process.env.EXPO_PUBLIC_ST3_TEST_PAIR_LINK ?? null);
    const subscription = Linking.addEventListener('url', event => handle(event.url));
    return () => subscription.remove();
  }, [completePairing]);

  const leave = useCallback(async () => {
    if (mode.kind === 'demo') await AsyncStorage.removeItem(DEMO_KEY);
    else {
      await SecureStore.deleteItemAsync(CREDENTIAL_KEY);
      await AsyncStorage.removeItem(PROJECTION_CACHE_KEY).catch(() => {});
    }
    setMode({ kind: 'connect' });
  }, [mode.kind]);

  const saveUrl = async () => {
    const normalized = normalizeGatewayUrl(urlDraft);
    if (!normalized) { setIssue('Enter the paired gateway HTTPS URL, or http:// with a Tailscale address (100.x), a .local name, or a private LAN address (10.x, 172.16-31.x, 192.168.x).'); return; }
    await AsyncStorage.setItem(URL_KEY, normalized);
    setUrl(normalized); setIssue('');
  };
  const enterDemo = async () => { await AsyncStorage.setItem(DEMO_KEY, '1'); setMode({ kind: 'demo' }); };

  const content = useMemo(() => {
    switch (mode.kind) {
      case 'demo': return <DemoApp leave={leave} />;
      case 'live': return <LiveApp key={mode.url} url={mode.url} credential={mode.credential} leave={leave} />;
      default: return null;
    }
  }, [mode, leave]);
  if (content) return <SafeAreaProvider>{content}</SafeAreaProvider>;

  return (
    <SafeAreaProvider>
      <SafeAreaView style={ui.screen}>
        <ScrollView contentContainerStyle={ui.scroll} keyboardShouldPersistTaps="handled">
          <Text style={local.brand}>Small Talk</Text>
          {mode.kind === 'starting' ? <Dim>Starting…</Dim> : (
            <>
              <Body color="subtext0">Connect to your paired-only gateway: its HTTPS URL; http:// with the host's Tailscale address (100.x.y.z), which Tailscale encrypts; or http:// with its .local name or private LAN address, which is not encrypted. Begin pairing on a trusted st machine, then enter its short-lived ID and code.</Body>
              <TextInput style={local.input} autoCapitalize="none" autoCorrect={false} keyboardType="url" keyboardAppearance="dark" placeholder="https://gateway, http://100.x.y.z:port, or http://host.local:port" placeholderTextColor={colors.overlay0} value={urlDraft} onChangeText={setUrlDraft} />
              {gatewayTransport(urlDraft) === 'lan' ? <Body color="yellow">{LAN_HTTP_WARNING}</Body> : null}
              <Buttons><Button label="Save gateway" onPress={() => void saveUrl()} /></Buttons>
              {url ? (
                <>
                  <TextInput style={local.input} autoCapitalize="none" keyboardAppearance="dark" placeholder="Pairing ID" placeholderTextColor={colors.overlay0} value={pairingId} onChangeText={setPairingId} />
                  <TextInput style={local.input} autoCapitalize="none" keyboardAppearance="dark" placeholder="Pairing code" placeholderTextColor={colors.overlay0} value={pairingCode} onChangeText={setPairingCode} />
                  <Buttons><Button label="Pair device" filled disabled={busy || !pairingId.trim() || !pairingCode.trim()} onPress={() => void completePairing(url, pairingId.trim(), pairingCode.trim())} /></Buttons>
                </>
              ) : null}
              {issue ? <Body color="red">{issue}</Body> : null}
              <View style={local.demo}>
                <Body>Or look around first: the demo shows an invented fleet, and nothing you do in it is sent anywhere.</Body>
                <Buttons><Button label="Explore the demo" color="yellow" onPress={() => void enterDemo()} /></Buttons>
              </View>
            </>
          )}
        </ScrollView>
      </SafeAreaView>
    </SafeAreaProvider>
  );
}

const local = StyleSheet.create({
  brand: { color: colors.text, fontSize: 28, fontWeight: '800', marginVertical: 8 },
  input: { color: colors.text, backgroundColor: colors.mantle, borderRadius: 10, padding: 12, fontSize: 15 },
  demoIcon: { width: 24, alignItems: 'center' },
  demoTag: { position: 'absolute', left: 14, top: -6, width: 30, alignItems: 'center', backgroundColor: colors.yellow, borderRadius: 4 },
  demoTagText: { color: colors.crust, fontSize: 9, fontWeight: '800' },
  demo: { marginTop: 24, gap: 8, padding: 14, borderRadius: 12, backgroundColor: colors.mantle },
});
