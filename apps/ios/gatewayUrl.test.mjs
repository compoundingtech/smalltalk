import assert from 'node:assert/strict';
import { normalizeGatewayUrl } from './gatewayUrl.ts';

// HTTPS gateways stay supported anywhere.
assert.equal(normalizeGatewayUrl(' https://gateway.example.ts.net:8443/ '), 'https://gateway.example.ts.net:8443');
// Plain HTTP is accepted only for Tailscale's IPv4 range, 100.64.0.0/10.
assert.equal(normalizeGatewayUrl('http://100.64.0.1:4102'), 'http://100.64.0.1:4102');
assert.equal(normalizeGatewayUrl('http://100.127.255.254:4102/'), 'http://100.127.255.254:4102');
for (const rejected of [
  'http://100.63.255.255:4102', // just below the range
  'http://100.128.0.1:4102', // just above the range
  'http://11.0.0.5:4102', // public, not RFC1918
  'http://gateway.example.com',
  'http://100.64.0.1.evil.example:4102',
  'http://[fd7a:115c:a1e0::1]:4102',
  'http://user:pass@100.64.0.1:4102',
  'ftp://100.64.0.1',
  'not a url',
  '',
]) assert.equal(normalizeGatewayUrl(rejected), null, rejected);

// LAN gateways: .local names and RFC1918 IPv4 over plain HTTP, labelled so the UI can warn.
const { gatewayTransport } = await import('./gatewayUrl.ts');
for (const [url, normalized] of [
  ['http://Silber.local:4102/', 'http://silber.local:4102'],
  ['http://studio-mac.local:4102', 'http://studio-mac.local:4102'],
  ['http://10.0.0.5:4102', 'http://10.0.0.5:4102'],
  ['http://172.16.0.1:4102', 'http://172.16.0.1:4102'],
  ['http://172.31.255.254:4102', 'http://172.31.255.254:4102'],
  ['http://192.168.1.20:4102', 'http://192.168.1.20:4102'],
]) {
  assert.equal(normalizeGatewayUrl(url), normalized, url);
  assert.equal(gatewayTransport(normalized), 'lan', url);
}
assert.equal(gatewayTransport('http://100.64.0.1:4102'), 'tailnet');
assert.equal(gatewayTransport('https://gateway.example.ts.net:8443'), 'https');
for (const rejected of [
  'http://172.15.255.255:4102', // below 172.16/12
  'http://172.32.0.1:4102', // above 172.16/12
  'http://192.169.0.1:4102', // 192/8 is not private
  'http://192.0.2.10:4102',
  'http://8.8.8.8',
  'http://local', // bare suffix
  'http://silber.local.example.com:4102',
  'http://evil.com/.local',
  'http://user@silber.local:4102',
]) {
  assert.equal(normalizeGatewayUrl(rejected), null, rejected);
  assert.equal(gatewayTransport(rejected), null, rejected);
}
