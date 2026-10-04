// Starts only an isolated test member. Test pairing secrets go to a 0600 file, not stdout.
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, writeFileSync, readFileSync, mkdirSync, openSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const environment = Object.fromEntries(Object.entries(process.env).filter(([name]) => !name.startsWith('ST_') && !name.startsWith('ST3_') && !name.startsWith('FABRIC_')));
const kind = 'st-fabric-isolated-proof-v1';
const command = (binary, args) => {
  const result = spawnSync(binary, args, { env: environment, encoding: 'utf8' });
  if (result.status !== 0) throw new Error(`${binary} test command failed: ${result.stderr}`);
  return result.stdout;
};

if (process.argv[2] === 'grant') {
  const file = process.argv[3], phone = process.argv[4];
  if (!file || !/^[a-f0-9]{64}$/i.test(phone ?? '')) throw new Error('grant requires the isolated member descriptor and phone NodeID');
  const member = JSON.parse(readFileSync(file, 'utf8'));
  if (member.kind !== kind || !member.root.startsWith('/tmp/st-fabric-proof-')) throw new Error('This is not an isolated proof member');
  command(member.fabric, ['--home', member.home, 'add', phone, 'demo-phone', '--allow', member.service]);
  command(member.fabric, ['--home', member.home, 'reload-peers']);
  const paired = JSON.parse(command(member.st, ['--endpoint', member.socket, 'devices', '--as', 'person/demo', 'pair', 'Demo fabric simulator', '--json']));
  const challenge = paired.value;
  const link = new URL('com.compoundingtech.smalltalk.starter://fabric-proof');
  link.searchParams.set('node', member.node);
  link.searchParams.set('service', member.service);
  link.searchParams.set('addr', JSON.stringify(member.addr));
  link.searchParams.set('id', challenge.pairing_id);
  link.searchParams.set('code', challenge.code);
  const output = join(member.root, 'pair-link.txt');
  writeFileSync(output, link.toString(), { mode: 0o600 });
  console.log(`Test pairing link written privately to ${output}`);
  process.exit(0);
}

const st = process.argv[2], fabric = process.argv[3];
if (!st || !fabric) throw new Error('Usage: node proof-member.mjs /path/to/st /path/to/fabric');
if (command(fabric, ['--version']).trim() !== '0.2.30+8bd9017') throw new Error('This proof requires fabric 0.2.30+8bd9017');
const root = mkdtempSync('/tmp/st-fabric-proof-');
const home = join(root, 'fabric'), state = join(root, 'state');
mkdirSync(home, { mode: 0o700 });
const socket = join(root, 'st.sock'), gateway = join(root, 'client.sock');
const config = join(root, 'config.toml');
writeFileSync(config, 'node = "demo-member"\nperson = "person/demo"\n', { mode: 0o600 });
const log = openSync(join(root, 'member.log'), 'a', 0o600);
const member = spawn(st, ['up', '--config', config, '--state-dir', state, '--socket', socket, '--client-gateway-socket', gateway], { env: environment, stdio: ['ignore', log, log] });
const transport = spawn(fabric, ['--home', home, 'up', '--foreground', '--server-session-detached-ttl-secs', '30'], { env: environment, stdio: ['ignore', log, log] });
const stop = () => { member.kill('SIGTERM'); transport.kill('SIGTERM'); };
process.once('SIGINT', stop); process.once('SIGTERM', stop);
try {
  let addr;
  // The daemon permits up to 106 seconds for slow login-shell startup.
  for (let attempt = 0; attempt < 1500; attempt++) {
    if (member.exitCode !== null || transport.exitCode !== null) throw new Error(`An isolated daemon exited; read ${root}/member.log`);
    if (existsSync(gateway)) {
      try { addr = JSON.parse(command(fabric, ['--home', home, 'addr'])); break; } catch { /* wait for its control socket */ }
    }
    await delay(100);
  }
  if (!addr) throw new Error('Isolated test member did not start');
  const service = 'demo-client/0';
  command(fabric, ['--home', home, 'expose', service, '--socket', gateway, '--ephemeral']);
  const description = { kind, root, home, state, socket, gateway, node: addr.id, addr, service, st, fabric };
  const descriptor = join(root, 'member.json');
  writeFileSync(descriptor, JSON.stringify(description), { mode: 0o600 });
  const link = new URL('com.compoundingtech.smalltalk.starter://fabric-proof');
  link.searchParams.set('node', addr.id); link.searchParams.set('service', service); link.searchParams.set('addr', JSON.stringify(addr));
  writeFileSync(join(root, 'identity-link.txt'), link.toString(), { mode: 0o600 });
  console.log(`Isolated proof member: ${descriptor}`);
  console.log('Open identity-link.txt in the Debug app, then grant its public phone node with this script.');
} catch (error) { stop(); throw error; }
