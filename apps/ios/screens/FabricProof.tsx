import { useEffect, useRef, useState, type ReactNode } from 'react';
import { AppState, ScrollView, Share, View } from 'react-native';
import * as Crypto from 'expo-crypto';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { API_VERSION, ClientError, St3Client } from '../../../clients/typescript/st3-client';
import { dialFabric, fabricIdentity, fabricStats, stopFabric } from '../modules/st-fabric';
import type { FabricProfile, FabricProofInput } from '../fabricProof';
import { FabricSession, nativeRefusedAfter, type ProofState } from '../fabricSession';
import { Button, Note, Screen, SectionHeader, T } from '../ui';

type Sample = { phase: string; at: string; native: Record<string, unknown> };

/** Temporary full client: no saved URL, credential, signing key or projection cache is changed. */
export function FabricProofScreen({ input, onClose, renderClient }: {
  input: FabricProofInput; onClose(): void; renderClient(profile: FabricProfile): ReactNode;
}) {
  const insets = useSafeAreaInsets();
  const [node, setNode] = useState(''), [result, setResult] = useState('Starting fabric proof…');
  const [profile, setProfile] = useState<FabricProfile | null>(null);
  const [details, setDetails] = useState(false), [report, setReport] = useState('');
  const latest = useRef<ProofState | null>(null), samples = useRef<Sample[]>([]);

  const capture = async (phase: string) => {
    try {
      const native = await fabricStats();
      samples.current = [...samples.current.slice(-31), { phase, at: new Date().toISOString(), native }];
      // No pairing link, bearer, listener URL, member address, message content or NodeID.
      const text = JSON.stringify({ wirePin: 'v0.2.30 / 8bd9017', iroh: '1.0.2', mode: input.mode ?? 'auto',
        events: latest.current?.events ?? [], samples: samples.current,
        batteryLimit: 'Coarse device battery fraction, not energy attribution; charging samples cannot establish battery cost.' }, null, 2);
      setReport(text);
    } catch { setReport('Measurements are unavailable on this build.'); }
  };

  useEffect(() => {
    let current = true;
    let admissionFloor = 0;
    samples.current = []; latest.current = null; setProfile(null); setReport(''); setResult('Starting fabric proof…');
    const held = new FabricSession({
      dial: () => dialFabric(input.node, input.service, input.address, input.mode),
      pair: async url => {
        const unpaired = new St3Client({ baseUrl: url });
        let refused = false;
        try { await unpaired.capabilities(); }
        catch (error) { if (error instanceof ClientError && [401, 403].includes(error.status)) refused = true; else throw error; }
        if (!refused) throw new Error('Test gateway accepted an unpaired client');
        const publicKey = Array.from(Crypto.getRandomBytes(32), byte => byte.toString(16).padStart(2, '0')).join('');
        const pairing = await unpaired.completePairing(input.id!, { api_version: API_VERSION, code: input.code!, device_public_key: publicKey });
        return pairing.value.credential;
      },
      authenticate: async (url, credential) => {
        try {
          const caps = await new St3Client({ baseUrl: url, credential: () => credential }).capabilities();
          return caps.value.session_actor;
        } catch (error) {
          if (nativeRefusedAfter(await fabricStats(), admissionFloor)) await held.refuse();
          throw error;
        }
      },
      stop: stopFabric,
    }, state => {
      if (!current) return;
      latest.current = state;
      if (input.fullClient && state.url && state.credential) setProfile({
        url: state.url, credential: state.credential, ready: state.ready, close: onClose,
        record: (event, fields) => {
          held.record(event, fields);
          if (event === 'feed' && fields?.state === 'reconnecting') void fabricStats().then(stats => {
            if (current && nativeRefusedAfter(stats, admissionFloor)) return held.refuse();
          }).catch(() => {});
        },
      });
      if (state.ready) setResult(`Capabilities received\nUnpaired client: refused\nAPI: ${API_VERSION}\nActor: ${state.actor}\nWire pin: v0.2.30 / 8bd9017\nIroh: 1.0.2`);
      else setResult(state.issue || 'Fabric session is stopped or connecting.');
    });
    held.record('input', { pairing: !!input.id, fullClient: !!input.fullClient });
    const run = async () => {
      try {
        const own = await fabricIdentity();
        if (!current) return;
        setNode(own);
        console.info(`Fabric proof public node: ${own}`);
        if (!input.id || !input.code) {
          setResult('Grant this public node ID only the test gateway service, then open a proof pairing link. Add client=1 to use Home and conversations.');
          return;
        }
        const before = await fabricStats();
        if (Array.isArray(before.attempts)) admissionFloor = Math.max(0, ...before.attempts.map(a => typeof a?.id === 'number' ? a.id : 0));
        held.record('lifecycle', { state: AppState.currentState ?? 'unknown' });
        await held.foreground(AppState.currentState === 'active');
        if (current && nativeRefusedAfter(await fabricStats(), admissionFloor)) await held.refuse();
        if (current) await capture('initial');
        if (!input.fullClient) await held.close();
      } catch (error) { if (current) setResult(error instanceof Error ? error.message : 'Fabric proof failed'); }
    };
    if (__DEV__) void run();
    const lifecycle = AppState.addEventListener('change', state => {
      held.record('lifecycle', { state });
      if (!input.id || !input.fullClient) { if (state !== 'active') void held.close(); return; }
      void held.foreground(state === 'active').then(() => { if (current) void capture(state); });
    });
    return () => { current = false; lifecycle.remove(); void held.close(); };
  }, [input]);

  const reportPanel = <ScrollView contentContainerStyle={{ padding: 16, gap: 12 }}>
    <SectionHeader title="Fabric development proof" />
    <T selectable>{result}</T>
    <T selectable>Phone node: {node || 'unavailable'}</T>
    <Note>Measurements contain no credentials or addresses. Battery fraction is a coarse sample; charging obscures battery drain. Message timing ends at the member's acknowledgement, not an agent reply.</Note>
    <Button label="Capture measurements" onPress={() => void capture('manual')} />
    <Button label="Share measurements" onPress={() => void Share.share({ message: report })} />
    {report ? <T selectable>{report}</T> : null}
    <Button label="Close fabric proof" onPress={onClose} />
  </ScrollView>;

  if (!profile) return <Screen><View style={{ flex: 1, paddingTop: insets.top }}>{reportPanel}</View></Screen>;
  return <View style={{ flex: 1 }}>
    <View style={{ paddingTop: insets.top, paddingHorizontal: 12, paddingBottom: 6, gap: 6 }}>
      <T>Fabric proof · {input.mode ?? 'auto'} · {profile.ready ? 'connected' : latest.current?.issue ? 'stopped' : 'reconnecting'}</T>
      {latest.current?.issue ? <T>{latest.current.issue}</T> : null}
      <View style={{ flexDirection: 'row', gap: 12 }}>
        <Button label={details ? 'Back to app' : 'Measurements'} onPress={() => { setDetails(!details); void capture('manual'); }} />
        <Button label="Exit proof" onPress={onClose} />
      </View>
    </View>
    <View style={{ flex: 1, display: details ? 'none' : 'flex' }}>{renderClient(profile)}</View>
    {details ? reportPanel : null}
  </View>;
}
