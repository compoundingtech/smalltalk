import { useCallback, useEffect, useRef, useState } from 'react';
import { useFocusEffect } from '@react-navigation/native';
import { plainError, type ClientConnection } from '../../../clients/typescript/st3-client';
import { ActionSheetIOS, Alert, ScrollView, Switch, Text, View } from 'react-native';
import { Banners, StatusLine, useDebugScroll, useListsOnFocus, useRefresh } from '../chrome';
import { gatewayTransport, LAN_HTTP_WARNING } from '../gatewayUrl';
import { clientDetail, clientTitle, deviceDetail, deviceTitle, olderThanMember } from '../presentation';
import { isUnmanaged, isUnresolved } from '@smalltalk/st3-views';
import { useStore } from '../store';
import { TABS } from '../tabs';
import { theme } from '../theme';
import { Button, Field, ListRow, Note, Screen, SectionHeader, T } from '../ui';
import { QualitySection } from './Quality';
import { UsageCard } from './Usage';
import { UNPINNED_WARNING } from '../pairingProof';
import { fabricTargetLabel } from '../fabricTarget';

function machineGlyph(state: string): { glyph: string; color: string } {
  if (state === 'local' || state === 'reachable' || state === 'dial-out') return { glyph: '●', color: theme.idle };
  if (state === 'unreachable') return { glyph: '✕', color: theme.fault };
  return { glyph: '?', color: theme.quiet };
}

// Fleet holds the optional clients window while focused. Older daemons read on focus or pull.
function useClients() {
  const { client, feed, inventoryGranted } = useStore();
  const [clients, setClients] = useState<ClientConnection[] | null>(null);
  const [issue, setIssue] = useState('');
  const [hasMore, setHasMore] = useState(false);
  const read = useCallback(async () => {
    if (!client || inventoryGranted !== false) return;
    try { setClients((await client.clientsList()).value.items); setIssue(''); }
    catch (error) {
      const text = plainError(error);
      setIssue(/404|not-found|it is gone/i.test(text) ? 'This st does not list its clients yet: its daemon needs an update.' : `Could not read connected clients: ${text}`);
    }
  }, [client, inventoryGranted]);
  useFocusEffect(useCallback(() => {
    if (inventoryGranted && feed) return feed.watchInventory('clients', { onRows: (rows, more) => { setClients(rows); setHasMore(more); }, onIssue: setIssue });
    if (inventoryGranted === false) void read();
  }, [feed, inventoryGranted, read]));
  return { clients, issue, read, hasMore };
}

// Fleet: machines, what runs where, this connection, who is connected, and the paired devices.
export function FleetScreen() {
  const { data, truncated, caps, url, gatewayMachineId, gatewayHost, order, actions, simpleOn, carrierInfo, inventoryGranted } = useStore();
  useListsOnFocus(['machines', 'devices', 'sessions']);
  const { clients, issue: clientsIssue, read: readClients, hasMore: clientsPartial } = useClients();
  const refresh = useRefresh(['machines', 'devices', 'sessions'], readClients);
  const scroll = useRef<ScrollView>(null);
  useDebugScroll(scroll as never);
  const now = Date.now();
  const undeclared = data.sessions.filter(session => session.state === 'running' && isUnmanaged(session));
  const gatewayListed = data.machines.some(machine => machine.id === gatewayMachineId);
  const found = <>
    {undeclared.map(session => <T key={session.id} dim style={{ paddingLeft: 34 }}>? {isUnresolved(session) ? 'unresolved process' : 'native session'} · {session.driver ?? 'harness'}{session.process ? ` · pid ${session.process.pid}` : ''}</T>)}
    {!undeclared.length ? <T dim style={{ paddingLeft: 34 }}>no sessions found running outside st</T> : null}
  </>;
  return <Screen>
    <Banners />
    <ScrollView ref={scroll} contentInsetAdjustmentBehavior="automatic" refreshControl={refresh} contentContainerStyle={{ paddingBottom: 32 }}>
      <StatusLine />
      {inventoryGranted === false ? <Note>This member’s inventories are read, not followed.</Note> : null}
      <UsageCard />
      <QualitySection />
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
      <SectionHeader title="conversations" />
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
        <T dim selectable>{carrierInfo.route.kind === 'fabric' ? `saved gateway, if fabric cannot connect · ${url}` : url}</T>
        {gatewayTransport(url) === 'lan' ? <T color={theme.waiting}>{LAN_HTTP_WARNING}</T> : null}
        <T>route · {carrierInfo.routeText}</T>
        {carrierInfo.route.fellBack && carrierInfo.route.why ? <T color={theme.waiting}>{carrierInfo.route.why}</T> : null}
        {carrierInfo.issue ? <T color={theme.fault}>{carrierInfo.issue}</T> : null}
        <NotificationsSetting />
        <Button label="forget this device" color={theme.red} onPress={() => Alert.alert('Forget this device?', 'You will need to pair again.', [{ text: 'Cancel', style: 'cancel' }, { text: 'Forget', style: 'destructive', onPress: () => void actions.forget() }])} />
      </View>
      <CarrierSection />
      <SectionHeader title="connected clients" count={clients?.length} />
      {clientsIssue ? <Note tone="warning">{clientsIssue}</Note> : null}
      {clientsPartial ? <Note>More connected clients exist beyond this window.</Note> : null}
      {!clients && !clientsIssue ? <Note>Loading connected clients…</Note> : null}
      {clients && !clients.length ? <T dim style={{ paddingHorizontal: 12 }}>No clients connected.</T> : null}
      {clients?.map(item => <ListRow
        key={`${item.actor} ${item.client ?? ''} ${item.via}`}
        glyph={item.connected ? '●' : '○'}
        glyphColor={item.connected ? theme.idle : theme.quiet}
        title={clientTitle(item, caps?.session_actor)}
        right={olderThanMember(item.client, caps?.machine_version) ? <T dim>older than this member</T> : undefined}
        second={clientDetail(item, now)}
      />)}
      <SectionHeader title="devices" count={data.devices.length} />
      {data.devices.map(device => <ListRow key={device.id} title={deviceTitle(device, caps?.session_actor)} second={`${deviceDetail(device, now)} · ${device.scopes.join(', ')}${device.push ? device.push.registered ? ` · push registered${device.push.environment ? ` (${device.push.environment})` : ''}` : ' · push not registered' : ''}`} />)}
      <SectionHeader title="tab order" />
      {order.map(tab => <ListRow key={tab} title={tab} right={<T dim>{order.indexOf(tab) + 1}</T>} onPress={() => ActionSheetIOS.showActionSheetWithOptions(
        { title: `Move ${tab}`, options: ['Move up', 'Move down', 'Cancel'], cancelButtonIndex: 2, disabledButtonIndices: [...(order.indexOf(tab) === 0 ? [0] : []), ...(order.indexOf(tab) === TABS.length - 1 ? [1] : [])] },
        index => { if (index === 0) actions.moveTab(tab, -1); if (index === 1) actions.moveTab(tab, 1); },
      )} />)}
    </ScrollView>
  </Screen>;
}

function NotificationsSetting() {
  const { pushInfo, actions, status } = useStore();
  const detail = !pushInfo.loaded ? 'Checking notification support…'
    : pushInfo.cleanup === 'pending' ? 'Remote opt-out is pending. Reconnect to previously registered members before forgetting this phone.'
    : pushInfo.cleanup === 'failed' ? 'Could not confirm remote opt-out. Pairing credentials are kept; reconnect and retry.'
    : !pushInfo.environment ? 'Requires a push-enabled build on a physical iPhone.'
    : status !== 'online' ? 'Connect to your paired member to register. Turning off is saved on this phone.'
    : !pushInfo.granted ? 'Push registration is unavailable on this paired member.'
    : pushInfo.status === 'failed' ? 'Could not update registration. Reconnect or turn notifications off and on to try again.'
    : pushInfo.status === 'permission-denied' ? 'Allow notifications in iOS Settings, or turn this setting off.'
    : pushInfo.status === 'working' ? 'Updating registration…'
    : pushInfo.status === 'registered' ? `Registered (${pushInfo.environment}). No alerts are sent yet.`
    : pushInfo.enabled ? 'Registration is waiting for this connection. No alerts are sent yet.'
    : 'Off by default. Register this phone for future alerts; no alerts are sent yet.';
  return <View style={{ paddingVertical: 12, gap: 5 }}>
    <View style={{ flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between', gap: 12 }}>
      <Text style={{ color: theme.text, fontSize: 17 }}>Notifications</Text>
      <Switch value={pushInfo.enabled} disabled={!pushInfo.enabled && !pushInfo.canEnable} onValueChange={on => void actions.setPushEnabled(on)} accessibilityLabel="Notifications on this phone" />
    </View>
    <Text style={{ color: theme.subtext0, fontSize: 13 }}>{detail}</Text>
  </View>;
}

// The carrier: how this phone reaches its member. The saved gateway (Tailscale, the local network or
// HTTPS) is the default. Fabric is an opt-in, offered only where this build links its bridge, and a
// fabric that cannot be used falls back to the saved gateway with the reason shown above. Fabric
// walks the members in the order listed here; a member that refuses stops the walk, and the person
// picks another member by hand. Each member is paired on its own (the device's own member uses the
// device's pairing), and the walk skips a member this phone is not paired with yet.
const PAIRING_TEXT = { device: "this device's pairing", own: 'paired on its own', none: 'not paired yet, so the walk skips it' } as const;

function CarrierSection() {
  const { carrierInfo, actions, error, busy } = useStore();
  const [draft, setDraft] = useState('');
  const [nameDraft, setNameDraft] = useState('');
  const [pairing, setPairing] = useState({ id: '', code: '', fingerprint: '' });
  const members = carrierInfo.members;
  const fabric = carrierInfo.fabric;
  const memberMenu = (index: number) => {
    const member = members[index];
    const unpaired = carrierInfo.pairings[index] === 'none';
    // The device's own member is paired by pairing the device; any other member on its own.
    const first = unpaired ? (member.device ? null : 'Pair this phone with it') : 'Use this member';
    const options = [...(first ? [first] : []), 'Move up', 'Move down', 'Rename', 'Remove', 'Cancel'];
    const at = first ? 0 : -1;
    ActionSheetIOS.showActionSheetWithOptions({ title: member.name, options, destructiveButtonIndex: options.length - 2, cancelButtonIndex: options.length - 1 }, picked => {
      const choice = picked - at;
      if (choice === 0) { if (unpaired) { setPairing({ id: '', code: '', fingerprint: '' }); actions.startMemberPairing(index); } else actions.pickFabricMember(index); }
      else if (choice === 1) void actions.moveFabricMember(index, -1);
      else if (choice === 2) void actions.moveFabricMember(index, 1);
      else if (choice === 3) Alert.prompt('Rename member', 'The name this phone shows for the member, also when it refuses.', name => void actions.renameFabricMember(index, name), 'plain-text', member.name);
      else if (choice === 4) void actions.removeFabricMember(index);
    });
  };
  useEffect(() => { if (carrierInfo.built) void actions.loadFabricNode(); }, [carrierInfo.built]);
  if (!carrierInfo.built) return null;
  const fabricOn = carrierInfo.choice === 'fabric';
  return <>
    <SectionHeader title="carrier" />
    <ListRow
      glyph={fabricOn ? '●' : '○'}
      glyphColor={fabricOn ? theme.green : theme.overlay1}
      title={`Fabric · ${fabricOn ? 'on' : 'off'} (experimental)`}
      second={fabricOn ? 'this phone reaches its member over fabric; Tailscale stays saved' : 'off: this phone uses its saved gateway'}
      onPress={() => void actions.setCarrier(fabricOn ? 'tailscale' : 'fabric')}
      accessibilityLabel={`Fabric, ${fabricOn ? 'on' : 'off'}. Double-tap to turn ${fabricOn ? 'off' : 'on'}.`}
    />
    <View style={{ paddingHorizontal: 12, gap: 2 }}>
      {members.length ? <T dim>members · tried in this order; a refusal stops at that member</T> : <T dim>no fabric member saved</T>}
    </View>
    {members.map((member, index) => {
      const live = fabric.member === member.name && fabric.phase === 'ready';
      const refusedHere = fabric.member === member.name && fabric.phase === 'refused';
      const trying = fabric.member === member.name && fabric.phase === 'dialing';
      return <ListRow
        key={`${member.node}/${member.service}`}
        glyph={live ? '●' : refusedHere ? '✕' : trying ? '…' : '○'}
        glyphColor={live ? theme.green : refusedHere ? theme.fault : theme.overlay1}
        title={`${index + 1}. ${member.name}${live ? ' · in use' : refusedHere ? ' · refused' : trying ? ' · trying' : ''}`}
        second={`${fabricTargetLabel(member)} · ${PAIRING_TEXT[carrierInfo.pairings[index]]}`}
        onPress={() => memberMenu(index)}
        accessibilityLabel={`Member ${index + 1}, ${member.name}${live ? ', in use' : refusedHere ? ', refused this phone' : ''}. Double-tap to use, move, rename or remove it.`}
      />;
    })}
    {carrierInfo.pairingMember ? <View style={{ paddingHorizontal: 12, gap: 4 }}>
      <T soft>Pairing this phone with {carrierInfo.pairingMember.name} on its own. {fabric.phase === 'ready' && fabric.member === carrierInfo.pairingMember.name ? 'Fabric is connected to it.' : fabric.phase === 'failed' || fabric.phase === 'refused' ? fabric.reason : 'Connecting to it over fabric…'}</T>
      <T dim>On {carrierInfo.pairingMember.name}, start a pairing for this phone and enter its pairing ID, code and fingerprint. This phone's credential for {carrierInfo.pairingMember.name} goes only to {carrierInfo.pairingMember.name}.</T>
      <Field autoCapitalize="none" autoCorrect={false} spellCheck={false} placeholder="pairing/…" value={pairing.id} onChangeText={id => setPairing(p => ({ ...p, id }))} />
      <Field autoCapitalize="none" autoCorrect={false} spellCheck={false} secureTextEntry placeholder="code" value={pairing.code} onChangeText={code => setPairing(p => ({ ...p, code }))} />
      <Field autoCapitalize="none" autoCorrect={false} spellCheck={false} placeholder="sha256:… (the member's fingerprint)" value={pairing.fingerprint} onChangeText={fingerprint => setPairing(p => ({ ...p, fingerprint }))} />
      <Button label={`pair with ${carrierInfo.pairingMember.name}`} disabled={busy || !pairing.id.trim() || !pairing.code.trim() || !pairing.fingerprint.trim()} onPress={() => void actions.pairMember(pairing.id, pairing.code, pairing.fingerprint).then(done => { if (done) setPairing({ id: '', code: '', fingerprint: '' }); })} />
      <Button label="cancel" onPress={() => actions.cancelMemberPairing()} />
      {error ? <T color={theme.fault}>{error}</T> : null}
    </View> : null}
    <View style={{ paddingHorizontal: 12, gap: 2 }}>
      {fabric.tried > 1 ? <T dim>tried · {fabric.tried} of {members.length} members</T> : null}
      {fabricOn && (fabric.phase === 'failed' || fabric.phase === 'refused') ? <Button label={members.length > 1 ? 'try the list again from the top' : 'try fabric again'} onPress={() => actions.retryFabric()} /> : null}
      {carrierInfo.route.kind === 'fabric' ? <>
        <T dim>path · {carrierInfo.path === 'unknown' ? 'not measured yet' : carrierInfo.path}</T>
        <Button label="check the path" onPress={() => void actions.refreshFabricPath()} />
      </> : null}
    </View>
    <ListRow
      glyph={carrierInfo.fallback ? '●' : '○'}
      glyphColor={carrierInfo.fallback ? theme.green : theme.overlay1}
      title={`Fall back to the saved gateway · ${carrierInfo.fallback ? 'on' : 'off'}`}
      second="when fabric cannot connect or the member refuses it"
      onPress={() => void actions.setFabricFallback(!carrierInfo.fallback)}
    />
    <View style={{ paddingHorizontal: 12, gap: 4 }}>
      <Field autoCapitalize="none" autoCorrect={false} spellCheck={false} placeholder="node=…&service=…  (paste the member's fabric target)" value={draft} onChangeText={setDraft} />
      <Field autoCorrect={false} placeholder={`name, such as the member's machine (${members.length ? `member ${members.length + 1}` : 'member 1'} if empty)`} value={nameDraft} onChangeText={setNameDraft} />
      <Button label="add fabric member" onPress={() => void actions.saveFabricTarget(draft, nameDraft).then(saved => { if (saved) { setDraft(''); setNameDraft(''); } })} disabled={!draft.trim()} />
      {error && draft ? <T color={theme.fault}>{error}</T> : null}
      {carrierInfo.node ? <>
        <T dim>this phone's fabric node ID, for the member's grant (public)</T>
        <T selectable>{carrierInfo.node}</T>
      </> : null}
    </View>
  </>;
}

// Pairing: the one screen before a device has a credential.
export function PairScreen() {
  const { carrierInfo: fabricInfo, actions: fabricActions } = useStore();
  useEffect(() => { if (fabricInfo.built) void fabricActions.loadFabricNode(); }, [fabricInfo.built]);
  const { url, urlDraft, setUrlDraft, pairDraft, busy, actions, carrierInfo } = useStore();
  // Fabric chosen with a target saved (a build can carry one): a device with no gateway pairs over fabric alone.
  const overFabric = carrierInfo.built && carrierInfo.choice === 'fabric' && !!carrierInfo.target;
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
      {pairDraft ? <T soft>Pairing with {pairDraft.gateway}</T> : overFabric && !url ? <>
        <T soft>Pairing over Fabric with {carrierInfo.target ? fabricTargetLabel(carrierInfo.target) : ''}. {carrierInfo.fabric.phase === 'ready' ? 'Fabric is connected.' : carrierInfo.fabric.phase === 'failed' || carrierInfo.fabric.phase === 'refused' ? carrierInfo.fabric.reason : 'Connecting to Fabric…'}</T>
        {carrierInfo.node ? <><T dim>This phone's fabric node ID, for the member's grant (public):</T><T selectable>{carrierInfo.node}</T></> : null}
      </> : <>
      <Field autoCapitalize="none" autoCorrect={false} spellCheck={false} keyboardType="url" placeholder="https://gateway, http://100.x.y.z:port, or http://host.local:port" value={urlDraft} onChangeText={setUrlDraft} />
      {gatewayTransport(urlDraft) === 'lan' ? <T color={theme.waiting}>{LAN_HTTP_WARNING}</T> : null}
      <Button label="save gateway" onPress={() => void actions.saveUrl()} />
      </>}
      {url || pairDraft || overFabric ? <>
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
