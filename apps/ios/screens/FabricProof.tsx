import { useEffect, useState } from 'react';
import { AppState, ScrollView } from 'react-native';
import * as Crypto from 'expo-crypto';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { API_VERSION, ClientError, St3Client } from '../../../clients/typescript/st3-client';
import { dialFabric, fabricIdentity, stopFabric } from '../modules/st-fabric';
import type { FabricProofInput } from '../fabricProof';
import { Button, Note, Screen, SectionHeader, T } from '../ui';

/** Temporary test client; the ordinary route, bearer and display cache remain untouched. */
export function FabricProofScreen({ input, onClose }: { input: FabricProofInput; onClose: () => void }) {
  const insets = useSafeAreaInsets();
  const [node, setNode] = useState(''), [result, setResult] = useState('Starting fabric proof…');
  useEffect(() => {
    let current = true;
    const run = async () => {
      try {
        const own = await fabricIdentity();
        if (!current) return;
        setNode(own);
        console.info('Fabric proof phone node:', own); // Public identity only; never the secret or pairing link.
        if (!input.id || !input.code) {
          setResult('Grant this public node ID only the test gateway service, then open the proof link with a test pairing ID and code.');
          return;
        }
        const bridge = await dialFabric(input.node, input.service, input.address);
        if (!current) { await stopFabric(); return; }
        const unpaired = new St3Client({ baseUrl: bridge.url });
        let refused = false;
        try { await unpaired.capabilities(); }
        catch (error) {
          if (error instanceof ClientError && (error.status === 401 || error.status === 403)) refused = true;
          else throw error;
        }
        if (!refused) throw new Error('Test gateway accepted an unpaired client');
        if (!current) return;
        const publicKey = Array.from(Crypto.getRandomBytes(32), byte => byte.toString(16).padStart(2, '0')).join('');
        const pairing = await unpaired.completePairing(input.id, { api_version: API_VERSION, code: input.code, device_public_key: publicKey });
        if (!current) return;
        const client = new St3Client({ baseUrl: bridge.url, credential: () => pairing.value.credential });
        const caps = await client.capabilities();
        if (current) setResult(`Capabilities received\nUnpaired client: refused\nAPI: ${caps.api_version}\nActor: ${caps.value.session_actor}\nFabric: ${bridge.fabricVersion}\nIroh: ${bridge.irohVersion}`);
      } catch (error) {
        // Native errors omit host-local target paths; never log the pairing link or credential.
        if (current) setResult(error instanceof Error ? error.message : 'Fabric proof failed');
      } finally { if (input.id) await stopFabric(); }
    };
    if (__DEV__) void run();
    const lifecycle = AppState.addEventListener('change', state => {
      if (state !== 'active') { current = false; void stopFabric(); setResult('Proof stopped in the background. Open a fresh proof link to retry.'); }
    });
    return () => { current = false; lifecycle.remove(); void stopFabric(); };
  }, [input]);

  return <Screen><ScrollView contentContainerStyle={{ padding: 16, paddingTop: insets.top + 16, gap: 12 }}>
    <SectionHeader title="Fabric development proof" />
    <T selectable>{result}</T>
    <T selectable>Phone node: {node || 'unavailable'}</T>
    <Note>The proof uses a temporary client. Close it to return to the saved gateway.</Note>
    <Button label="Close fabric proof" onPress={onClose} />
  </ScrollView></Screen>;
}
