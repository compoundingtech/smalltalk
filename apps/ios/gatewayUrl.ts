// A paired gateway is reached over HTTPS, or over plain HTTP only on networks the user controls:
// a Tailscale IPv4 address (100.64.0.0/10, which Tailscale encrypts) or the local network
// (`*.local` names and RFC1918 IPv4). Plain HTTP on a LAN is not encrypted. Everything else stays
// HTTPS-only, matching the app's scoped App Transport Security exceptions.
export type GatewayTransport = 'https' | 'tailnet' | 'lan';

function ipv4(host: string): number[] | null {
  const parts = host.split('.');
  if (parts.length !== 4 || !parts.every(part => /^(0|[1-9]\d{0,2})$/.test(part) && Number(part) <= 255)) return null;
  return parts.map(Number);
}

export function isTailnetIPv4(host: string): boolean {
  const octets = ipv4(host);
  return !!octets && octets[0] === 100 && octets[1] >= 64 && octets[1] <= 127;
}

export function isPrivateLanHost(host: string): boolean {
  const octets = ipv4(host);
  if (octets) {
    const [first, second] = octets;
    return first === 10 || (first === 172 && second >= 16 && second <= 31) || (first === 192 && second === 168);
  }
  return /^([a-z0-9]([a-z0-9-]*[a-z0-9])?\.)+local$/.test(host);
}

export function gatewayTransport(input: string): GatewayTransport | null {
  let url: URL;
  try { url = new URL(input.trim()); } catch { return null; }
  if (url.username || url.password) return null;
  if (url.protocol === 'https:') return 'https';
  if (url.protocol !== 'http:') return null;
  if (isTailnetIPv4(url.hostname)) return 'tailnet';
  return isPrivateLanHost(url.hostname) ? 'lan' : null;
}

export function normalizeGatewayUrl(input: string): string | null {
  if (!gatewayTransport(input)) return null;
  const url = new URL(input.trim());
  return `${url.protocol}//${url.host}${url.pathname.replace(/\/+$/, '')}`;
}

export const LAN_HTTP_WARNING = 'Plain HTTP on a local network is not encrypted. Anyone on this network can read the paired credential and your st3 data. Use it only on a network you trust.';
