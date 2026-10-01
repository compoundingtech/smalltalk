import { useRef, useState } from 'react';
import { ActionSheetIOS, Alert, ScrollView, View } from 'react-native';
import { agentName } from '../agentsView';
import { Banners, StatusLine, useDebugScroll, useListsOnFocus, useRefresh } from '../chrome';
import { gatewayTransport, LAN_HTTP_WARNING } from '../gatewayUrl';
import { agentHealth, ago, deviceDetail, deviceTitle, queuedWorkSummary } from '../presentation';
import { isUnmanaged, isUnresolved } from '../sessionView';
import { useStore } from '../store';
import { TABS } from '../tabs';
import { theme } from '../theme';
import { Button, Field, ListRow, Note, Screen, SectionHeader, T } from '../ui';

function machineGlyph(state: string): { glyph: string; color: string } {
  if (state === 'local' || state === 'reachable' || state === 'dial-out') return { glyph: '●', color: theme.idle };
  if (state === 'unreachable') return { glyph: '✕', color: theme.fault };
  return { glyph: '?', color: theme.quiet };
}

// Fleet: machines, what runs where, this connection and the paired devices.
export function FleetScreen() {
  const { data, truncated, caps, url, gatewayMachineId, gatewayHost, order, actions, glassesOn } = useStore();
  useListsOnFocus(['machines', 'devices', 'sessions']);
  const refresh = useRefresh(['machines', 'devices', 'sessions']);
  const scroll = useRef<ScrollView>(null);
  useDebugScroll(scroll as never);
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
      <SectionHeader title="this connection" />
      <View style={{ paddingHorizontal: 12, gap: 2 }}>
        <T>{caps ? `${caps.session_actor} · ${caps.transport}` : 'reconnecting'}</T>
        <T dim selectable>{url}</T>
        {gatewayTransport(url) === 'lan' ? <T color={theme.waiting}>{LAN_HTTP_WARNING}</T> : null}
        <Button label="forget this device" color={theme.red} onPress={() => Alert.alert('Forget this device?', 'You will need to pair again.', [{ text: 'Cancel', style: 'cancel' }, { text: 'Forget', style: 'destructive', onPress: () => void actions.forget() }])} />
      </View>
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
  const { url, urlDraft, setUrlDraft, busy, actions } = useStore();
  const [id, setId] = useState(''), [code, setCode] = useState('');
  return <Screen>
    <Banners />
    <ScrollView contentInsetAdjustmentBehavior="automatic" contentContainerStyle={{ padding: 12, paddingBottom: 48 }} keyboardShouldPersistTaps="handled" keyboardDismissMode="interactive">
      <T soft>Use the paired-only gateway: its HTTPS URL; http:// with the host's Tailscale address (100.x.y.z), which Tailscale encrypts; or http:// with its .local name or private LAN address, which is not encrypted. Begin pairing on a trusted st machine, then enter its short-lived ID and code.</T>
      <Field autoCapitalize="none" keyboardType="url" placeholder="https://gateway, http://100.x.y.z:port, or http://host.local:port" value={urlDraft} onChangeText={setUrlDraft} />
      {gatewayTransport(urlDraft) === 'lan' ? <T color={theme.waiting}>{LAN_HTTP_WARNING}</T> : null}
      <Button label="save gateway" onPress={() => void actions.saveUrl()} />
      {url ? <>
        <SectionHeader title="pair" />
        <Field autoCapitalize="none" placeholder="pairing ID" value={id} onChangeText={setId} />
        <Field autoCapitalize="none" placeholder="pairing code" value={code} onChangeText={setCode} />
        <Button label="pair this device" disabled={busy} onPress={() => void actions.pair(id, code).then(done => { if (done) { setId(''); setCode(''); } })} />
      </> : null}
    </ScrollView>
  </Screen>;
}
