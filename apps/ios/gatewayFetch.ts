// The paired gateway sits behind a proxy (Tailscale Serve). When the proxy cannot reach st3 it
// answers with its own non-JSON body, often an empty 502, which the client would otherwise
// report as a JSON parse error. Name the gateway failure instead.
export function gatewayFetch(inner: typeof fetch = fetch): typeof fetch {
  return async (input, init) => {
    const response = await inner(input, init);
    const type = response.headers.get('content-type') ?? '';
    if (response.ok || type.includes('json')) return response;
    const route = new URL(String(input)).pathname;
    throw new Error(`The gateway returned HTTP ${response.status} with no st3 response for ${route}.`);
  };
}
