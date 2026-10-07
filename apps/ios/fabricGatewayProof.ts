import { API_VERSION } from '../../clients/typescript/st3-client';

// Pairing discovery is public; prove that an actual protected route still refuses this client.
export async function verifyUnpairedFabricGateway(url: string, fetchImpl: typeof fetch = fetch): Promise<void> {
  const response = await fetchImpl(`${url}/v1/client/agents?limit=1`, { headers: { Accept: 'application/json' } });
  const payload = await response.json();
  if (![401, 403].includes(response.status) || payload?.api_version !== API_VERSION || payload?.error_version !== 'st3.client.error.v0') {
    throw new Error('Test gateway did not refuse an unpaired client on a protected route');
  }
}
