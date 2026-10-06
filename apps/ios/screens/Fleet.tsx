import { useCallback, useEffect, useRef, useState } from 'react';
import { useFocusEffect } from '@react-navigation/native';
import { plainError, type ClientConnections } from '../../../clients/typescript/st3-client';
import { ActionSheetIOS, Alert, ScrollView, View } from 'react-native';
import { agentName } from '../agentsView';
import { Banners, StatusLine, useDebugScroll, useListsOnFocus, useRefresh } from '../chrome';
import { gatewayTransport, LAN_HTTP_WARNING } from '../gatewayUrl';
import { agentHealth, ago, clientDetail, clientTitle, deviceDetail, deviceTitle, olderThanMember, queuedWorkSummary } from '../presentation';
import { isUnmanaged, isUnresolved } from '@smalltalk/st3-views';
import { useStore } from '../store';
import { TABS } from '../tabs';
import { theme } from '../theme';
import { Button, Field, ListRow, Note, Screen, SectionHeader, T } from '../ui';
import { UsageCard } from './Usage';
import { UNPINNED_WARNING } from '../pairingProof';

function machineGlyph(state: string): { glyph: string; color: string } {
  if (state === 'local' || state === 'reachable' || state === 'dial-out') return { glyph: '●', color: theme.idle };
  if (state === 'unreachable') return { glyph: '✕', color: theme.fault };
  return { glyph: '?', color: theme.quiet };
}

// Who is connected to the member this phone talks to. st keeps no stream of it, so it is read on
// focus and again every few seconds while Fleet shows.
const CLIENTS_EVERY_MS = 10_000;
function useClients() {
  const { client } = useStore();
  const [clients, setClients] = useState<ClientConnections | null>(null);
  const [issue, setIssue] = useState('');
  const read = useCallback(async () => {
    if (!client) return;
    try {
      setClients((await client.clientsList()).value);
      setIssue('');
    } catch (error) {
      const text = plainError(error);
      setIssue(/404|not-found|it is gone/i.test(text) ? 'This st does not list its clients yet: its daemon needs an update.' : `Could not read connected clients: ${text}`);
    }
  }, [client]);
  useFocusEffect(useCallback(() => {
    void read();
    const timer = setInterval(() => void read(), CLIENTS_EVERY_MS);
    return () => clearInterval(timer);
  }, [read]));
  return { clients, issue };
}

// Fleet: machines, what runs where, this connection, who is connected, and the paired devices.
export function FleetScreen() {
  const { data, truncated, caps, url, gatewayMachineId, gatewayHost, order, actions, glassesOn, simpleOn } = useStore();
  useListsOnFocus(['machines', 'devices', 'sessions']);
  const refresh = useRefresh(['machines', 'devices', 'sessions']);
  const scroll = useRef<ScrollView>(null);
  useDebugScroll(scroll as never);
  const { clients, issue: clientsIssue } = useClients();
  const now = Date.now();
  const undeclared = data.sessions.filter(session => session.state === 'running' && isUnmanaged(session));
  const unhealthy = data.agents.filter(agent => !agentHealth(agent).healthy);
  const busy = data.agents.filter(agent => (agent.active_work_count ?? 0) > 0 || (agent.queued_work_count ?? 0) > 0);
  const gatewayListed = data.machines.some(machine => machine.id === gatewayMachineId);
  const found = <>
    {undeclared.map(session => <T key={session.id} dim style={{ paddingLeft: 34 }}>? {isUnresolved(session) ? 'unresolved process' : 'native session'} · {session.driver ?? 'harness'}{session.process ? ` · pid ${session.process.pid}` : ''}</T>)}
    {!undeclared.length ? <T dim style={{ paddingLeft: 34 }}>no sessions found running outside st</T> : null}
  </>;
  return <Screen>
    <Banners />
    <ScrollView ref={scroll} contentInsetAdjustmentBehavior="automatic" refreshControl={refresh} contentContainerStyle={{ paddingBottom: 32 }}>
      <StatusLine />
      <UsageCard />
      <SectionHeader title="machines" count={data.machines.length} />
      {data.machines.map(machine => <View key={machine.id}>
        <ListRow
          glyph={machineGlyph(machine.state).glyph}
          glyphColor={machineGlyph(machine.state).color}
          title={`${machine.name}${machine.id === gatewayMachineId ? ' · this gateway' : ''}`}
          right={<T dim>{machine.occupancy.running_runtimes} running</T>}
          second={`${machine.state} · ${machine.capacity.state} · ${machine.transports.map(t => `${t.protocol} ${t.status}`).join(' · ')}`}
        />
        {machine.id === gatewayMachineId ? found : null}
      </View>)}
      {!gatewayListed && gatewayHost ? <View><ListRow glyph="●" glyphColor={theme.idle} title={`${gatewayHost} · this gateway`} />{found}</View> : null}
      {truncated.machines || truncated.devices ? <Note tone="warning">This view is partial; the CLI lists every machine and device.</Note> : null}
      <SectionHeader title="agent work" count={busy.length + unhealthy.length} />
      {unhealthy.map(agent => <ListRow key={`unhealthy-${agent.id}`} glyph="✕" glyphColor={theme.fault} title={agentName(agent)} second={`${agentHealth(agent).label} · observed ${ago(agent.updated_at, now)} ago`} />)}
      {busy.map(agent => <ListRow key={agent.id} glyph="⠿" glyphColor={theme.working} title={agentName(agent)} right={<T dim>{agent.active_work_count ?? 0} active</T>} second={queuedWorkSummary(agent, now) ?? undefined} />)}
      <SectionHeader title="tabs" />
      <ListRow
        glyph={glassesOn ? '●' : '○'}
        glyphColor={glassesOn ? theme.green : theme.overlay1}
        title={`Spaces tab · ${glassesOn ? 'on' : 'off'}`}
        second="stui's spaces, their tabs and splits, one thing at a time"
        onPress={() => actions.setGlassesOn(!glassesOn)}
        accessibilityLabel={`Spaces, ${glassesOn ? 'on' : 'off'}. Double-tap to turn ${glassesOn ? 'off' : 'on'}.`}
      />
      <ListRow
        glyph={simpleOn ? '●' : '○'}
        glyphColor={simpleOn ? theme.green : theme.overlay1}
        title={`Simplified conversations · ${simpleOn ? 'on' : 'off'}`}
        second="a tool call to a line, a run of calls to one line; this phone only"
        onPress={() => actions.setSimpleOn(!simpleOn)}
      />
      <SectionHeader title="this connection" />
      <View style={{ paddingHorizontal: 12, gap: 2 }}>
        <T>{caps ? `${caps.session_actor} · ${caps.transport}` : 'reconnecting'}</T>
        <T dim selectable>{url}</T>
        {gatewayTransport(url) === 'lan' ? <T color={theme.waiting}>{LAN_HTTP_WARNING}</T> : null}
        <Button label="forget this device" color={theme.red} onPress={() => Alert.alert('Forget this device?', 'You will need to pair again.', [{ text: 'Cancel', style: 'cancel' }, { text: 'Forget', style: 'destructive', onPress: () => void actions.forget() }])} />
      </View>
      <SectionHeader title="connected clients" count={clients?.items.length} />
      {clientsIssue ? <Note tone="warning">{clientsIssue}</Note> : null}
      {!clients && !clientsIssue ? <Note>Loading connected clients…</Note> : null}
      {clients && !clients.items.length ? <T dim style={{ paddingHorizontal: 12 }}>No clients connected.</T> : null}
      {clients?.items.map(item => <ListRow
        key={`${item.actor} ${item.client ?? ''} ${item.via}`}
        glyph={item.connected ? '●' : '○'}
        glyphColor={item.connected ? theme.idle : theme.quiet}
        title={clientTitle(item, caps?.session_actor)}
        right={olderThanMember(item.client, caps?.machine_version) ? <T dim>older than this member</T> : undefined}
        second={clientDetail(item, now)}
      />)}
      <SectionHeader title="devices" count={data.devices.length} />
      {data.devices.map(device => <ListRow key={device.id} title={deviceTitle(device, caps?.session_actor)} second={`${deviceDetail(device, now)} · ${device.scopes.join(', ')}`} />)}
      <SectionHeader title="tab order" />
      {order.map(tab => <ListRow key={tab} title={tab} right={<T dim>{order.indexOf(tab) + 1}</T>} onPress={() => ActionSheetIOS.showActionSheetWithOptions(
        { title: `Move ${tab}`, options: ['Move up', 'Move down', 'Cancel'], cancelButtonIndex: 2, disabledButtonIndices: [...(order.indexOf(tab) === 0 ? [0] : []), ...(order.indexOf(tab) === TABS.length - 1 ? [1] : [])] },
        index => { if (index === 0) actions.moveTab(tab, -1); if (index === 1) actions.moveTab(tab, 1); },
      )} />)}
    </ScrollView>
  </Screen>;
}

// Pairing: the one screen before a device has a credential.
export function PairScreen() {
  const { url, urlDraft, setUrlDraft, pairDraft, busy, actions } = useStore();
  const [id, setId] = useState(pairDraft?.id ?? ''), [code, setCode] = useState(pairDraft?.code ?? '');
  const [fingerprint, setFingerprint] = useState(''), [unpinned, setUnpinned] = useState(false);
  useEffect(() => {
    if (pairDraft) { setId(pairDraft.id); setCode(pairDraft.code); setFingerprint(''); setUnpinned(false); }
  }, [pairDraft]);
  const submit = () => void actions.pair(id, code, fingerprint, unpinned).then(done => { if (done) { setId(''); setCode(''); setFingerprint(''); } });
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ padding: 12, paddingBottom: 48 }} keyboardShouldPersistTaps="handled" keyboardDismissMode="interactive">
      <T soft>Begin pairing on a trusted st machine, then enter its short-lived ID, code, and the person-root fingerprint copied separately. Use HTTPS or an already encrypted path such as Tailscale for the paired-only gateway.</T>
      {pairDraft ? <T soft>Pairing with {pairDraft.gateway}</T> : <>
      <Field autoCapitalize="none" autoCorrect={false} spellCheck={false} keyboardType="url" placeholder="https://gateway, http://100.x.y.z:port, or http://host.local:port" value={urlDraft} onChangeText={setUrlDraft} />
      {gatewayTransport(urlDraft) === 'lan' ? <T color={theme.waiting}>{LAN_HTTP_WARNING}</T> : null}
      <Button label="save gateway" onPress={() => void actions.saveUrl()} />
      </>}
      {url || pairDraft ? <>
        <SectionHeader title="pair" />
        <Field autoCapitalize="none" autoCorrect={false} spellCheck={false} placeholder="pairing ID" value={id} onChangeText={setId} />
        <Field autoCapitalize="none" autoCorrect={false} spellCheck={false} placeholder="pairing code" value={code} onChangeText={setCode} />
        <Field autoCapitalize="none" autoCorrect={false} spellCheck={false} placeholder="person-root fingerprint (sha256:…)" value={fingerprint} onChangeText={value => { setFingerprint(value); setUnpinned(false); }} />
        {unpinned ? <Note tone="warning">{UNPINNED_WARNING}</Note> : null}
        <Button label="pair this device" disabled={busy} onPress={submit} />
        <Button label={unpinned ? 'require fingerprint again' : 'pair without a trusted fingerprint…'} disabled={busy} onPress={() => {
          if (unpinned) { setUnpinned(false); return; }
          Alert.alert('Pair without identity verification?', UNPINNED_WARNING, [{ text: 'Cancel', style: 'cancel' }, { text: 'Use unpinned pairing', style: 'destructive', onPress: () => { setFingerprint(''); setUnpinned(true); } }]);
        }} />
        {pairDraft ? <Button label="cancel re-pairing" disabled={busy} onPress={actions.cancelPairDraft} /> : null}
      </> : null}
    </ScrollView>
  </Screen>;
}
