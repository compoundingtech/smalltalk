#!/usr/bin/env python3
"""Install a fixed macOS app with explicit signing configuration and bundle rollback."""
import argparse
import ctypes
import fcntl
import re
import ssl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile

CERTIFICATE = os.environ.get('ST_MACOS_SIGNING_IDENTITY') or None
TEAM = os.environ.get('ST_MACOS_SIGNING_TEAM') or None
APP = os.environ.get('ST_MACOS_APP_PATH') or None
BIN_DIR = os.environ.get('ST_MACOS_BIN_DIR') or None
IDENTIFIER = os.environ.get('ST_MACOS_BUNDLE_ID', 'com.compoundingtech.smalltalk')
SERVICES = ['com.compoundingtech.st3', 'com.compoundingtech.st3.replication']
LSREGISTER = '/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister'


def command(args):
    return subprocess.run([str(a) for a in args], check=True, timeout=60, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT).stdout


def sha(path):
    h = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def fixed(home):
    return Path(APP).expanduser().absolute() if APP else Path(home) / 'Applications/SmallTalk.app'


def binaries(app):
    return {name: Path(app) / 'Contents/MacOS' / name for name in ['st3', 'stui']}


def write_atomic(path, data, mode=0o644):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as stream:
        stream.write(data); stream.flush(); os.fsync(stream.fileno()); name = stream.name
    os.chmod(name, mode)
    os.replace(name, path)


def resolve_identity():
    global CERTIFICATE
    if not CERTIFICATE:
        if TEAM:
            raise RuntimeError('a configured signing team requires an explicit signing identity')
        return None
    configured = CERTIFICATE
    output = command(['/usr/bin/security', 'find-identity', '-v', '-p', 'codesigning'])
    identities = re.findall(r'^\s*\d+\)\s+([A-Fa-f0-9]{40}) "([^"]+)"', output, re.M)
    matches = [digest.upper() for digest, name in identities if configured.upper() == digest.upper() or configured == name]
    if len(matches) != 1:
        raise RuntimeError('configured signing identity was not found uniquely; refusing ad-hoc fallback: ' + configured)
    certificate = matches[0]
    if TEAM:
        certificates = command(['/usr/bin/security', 'find-certificate', '-a', '-p'])
        blocks = re.findall(r'-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----', certificates, re.S)
        pem = next((block for block in blocks if hashlib.sha1(ssl.PEM_cert_to_DER_cert(block)).hexdigest().upper() == certificate), None)
        if pem is None:
            raise RuntimeError('configured signing certificate could not be read to verify its team')
        with tempfile.NamedTemporaryFile('w') as stream:
            stream.write(pem); stream.flush()
            subject = command(['/usr/bin/openssl', 'x509', '-in', stream.name, '-noout', '-subject', '-nameopt', 'RFC2253'])
        if not re.search(r'(?:^|,)\s*OU=' + re.escape(TEAM) + r'(?:,|$)', subject.strip().split('subject=', 1)[-1]):
            raise RuntimeError('configured identity does not belong to the configured signing team')
    CERTIFICATE = certificate
    return certificate


def bin_dir(home):
    return Path(BIN_DIR).expanduser().absolute() if BIN_DIR else Path(home) / '.local/bin'


def verify(app, expected=None):
    certificate_id, team, identifier = (CERTIFICATE, TEAM, IDENTIFIER) if expected is None else (expected['certificate'], expected['team'], expected['identifier'])
    command(['/usr/bin/codesign', '--verify', '--strict', '--deep', app])
    output = command(['/usr/bin/codesign', '-d', '-r-', '--verbose=2', app])
    if 'Identifier=' + identifier + '\n' not in output:
        raise RuntimeError('signed bundle identifier changed')
    requirement = next((line.lstrip('# ') for line in output.splitlines() if line.lstrip('# ').startswith('designated => ')), None)
    if not requirement or (certificate_id and 'cdhash' in requirement):
        raise RuntimeError('bundle has no valid designated requirement for its signing configuration')
    if team and 'TeamIdentifier=' + team + '\n' not in output:
        raise RuntimeError('signed app does not belong to the configured signing team')
    certificate = None
    if certificate_id:
        with tempfile.TemporaryDirectory() as directory:
            prefix = Path(directory) / 'certificate'
            command(['/usr/bin/codesign', '-d', '--extract-certificates=' + str(prefix), app])
            certificate = hashlib.sha1(Path(str(prefix) + '0').read_bytes()).hexdigest().upper()
        if certificate != certificate_id:
            raise RuntimeError('bundle is not signed by the configured identity')
    return {'certificate': certificate, 'identifier': identifier, 'team': team,
            'designated_requirement': requirement, 'persistent_identity': bool(certificate_id)}


def prepare(home, job, built):
    resolve_identity()
    payload = {name: sha(path) for name, path in built.items()}
    desired = {'hashes': payload, 'certificate': CERTIFICATE, 'team': TEAM, 'identifier': IDENTIFIER}
    current = fixed(home)
    metadata = current / 'Contents/Resources/deploy-payload.json'
    if metadata.exists() and json.loads(metadata.read_text()) == desired:
        return current, dict(verify(current), payload_unchanged=True), payload
    app = Path(job) / 'candidate/St3.app'
    app.parent.mkdir(parents=True, exist_ok=True)
    if app.exists():
        shutil.rmtree(app)
    paths = binaries(app)
    paths['st3'].parent.mkdir(parents=True)
    resources = app / 'Contents/Resources'
    resources.mkdir()
    for name, path in built.items():
        shutil.copy2(path, paths[name]); os.chmod(paths[name], 0o755)
    info = {'CFBundleIdentifier': IDENTIFIER, 'CFBundleExecutable': 'st3', 'CFBundleName': 'St3',
            'CFBundlePackageType': 'APPL', 'CFBundleVersion': '1', 'LSUIElement': True}
    (app / 'Contents/Info.plist').write_bytes(plistlib.dumps(info))
    metadata = resources / 'deploy-payload.json'
    metadata.write_text(json.dumps(desired, sort_keys=True) + '\n')
    for name, path in paths.items():
        command(['/usr/bin/codesign', '--force', '--sign', CERTIFICATE or '-', '--identifier',
                 IDENTIFIER if name == 'st3' else IDENTIFIER + '.stui', '--timestamp=none', path])
    command(['/usr/bin/codesign', '--force', '--sign', CERTIFICATE or '-', '--identifier', IDENTIFIER, '--timestamp=none', app])
    identity = verify(app)
    return app, dict(identity, payload_unchanged=False), payload


def backup(home, job):
    previous = Path(job) / 'previous-app'
    current = fixed(home)
    if current.exists():
        metadata = json.loads((current / 'Contents/Resources/deploy-payload.json').read_text())
        verify(current, metadata)
        command(['/usr/bin/ditto', current, previous])
    plists = Path(job) / 'previous-launchd'
    plists.mkdir(exist_ok=True)
    for service in SERVICES:
        path = Path(home) / 'Library/LaunchAgents' / (service + '.plist')
        if path.exists():
            shutil.copy2(path, plists / path.name)
    entries = {}
    previous_bin = Path(job) / 'previous-bin'
    previous_bin.mkdir(exist_ok=True)
    for name in ['st3', 'stui', 'st']:
        path = bin_dir(home) / name
        if path.is_symlink():
            entries[name] = {'link': os.readlink(path)}
        elif path.exists():
            if not path.is_file():
                raise RuntimeError('refusing to replace non-file: ' + str(path))
            shutil.copy2(path, previous_bin / name)
            entries[name] = {'file': name, 'mode': path.stat().st_mode & 0o777}
        else:
            entries[name] = None
    write_atomic(Path(job) / 'previous-bin.json', json.dumps(entries).encode())
    return current.exists()


def exchange(staged, destination):
    # Both app directories are on the same Applications filesystem. The old
    # directory stays intact until the atomic swap succeeds.
    if not destination.exists():
        os.replace(staged, destination)
        return
    libc = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
    rename = libc.renamex_np
    rename.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint]
    rename.restype = ctypes.c_int
    if rename(os.fsencode(staged), os.fsencode(destination), 2):
        number = ctypes.get_errno()
        raise OSError(number, os.strerror(number))


def swap(app, home, job, suffix, expected=None):
    destination = fixed(home)
    destination.parent.mkdir(parents=True, exist_ok=True)
    if Path(app).resolve() == destination.resolve():
        return
    staged = destination.parent / ('.St3-' + Path(job).name + '-' + suffix + '.app')
    if staged.exists():
        shutil.rmtree(staged)
    command(['/usr/bin/ditto', app, staged])
    verify(staged, expected)
    exchange(staged, destination)
    if staged.exists():
        shutil.rmtree(staged)


def install(app, home, job):
    swap(app, home, job, 'install')
    links = dict(binaries(fixed(home)), st=Path('st3'))
    for name, path in links.items():
        link = bin_dir(home) / name
        link.parent.mkdir(parents=True, exist_ok=True)
        staged = link.parent / ('.' + name + '-' + Path(job).name)
        staged.unlink(missing_ok=True); staged.symlink_to(path); os.replace(staged, link)
    for service in SERVICES:
        path = Path(home) / 'Library/LaunchAgents' / (service + '.plist')
        if not path.exists():
            continue
        plist = plistlib.loads(path.read_bytes())
        if plist.get('ProgramArguments'):
            plist['ProgramArguments'][0] = str(binaries(fixed(home))['st3'])
        elif 'Program' in plist:
            plist['Program'] = str(binaries(fixed(home))['st3'])
        else:
            raise RuntimeError('launchd service has no executable: ' + str(path))
        write_atomic(path, plistlib.dumps(plist))
    command([LSREGISTER, '-f', fixed(home)])


def restore(home, job, had_app):
    if had_app:
        previous = Path(job) / 'previous-app'
        metadata = json.loads((previous / 'Contents/Resources/deploy-payload.json').read_text())
        swap(previous, home, job, 'rollback', metadata)
        command([LSREGISTER, '-f', fixed(home)])
    else:
        current = fixed(home)
        if current.exists():
            moved = Path(job) / 'failed-app'
            if moved.exists():
                shutil.rmtree(moved)
            os.replace(current, moved)
    for service in SERVICES:
        original = Path(job) / 'previous-launchd' / (service + '.plist')
        if original.exists():
            write_atomic(Path(home) / 'Library/LaunchAgents' / original.name, original.read_bytes())

    for name, prior in json.loads((Path(job) / 'previous-bin.json').read_text()).items():
        path = bin_dir(home) / name
        if prior is None:
            path.unlink(missing_ok=True)
        elif 'link' in prior:
            staged = path.parent / ('.' + name + '-' + Path(job).name + '-restore')
            staged.unlink(missing_ok=True)
            staged.symlink_to(prior['link'])
            os.replace(staged, path)
        else:
            write_atomic(path, (Path(job) / 'previous-bin' / name).read_bytes(), prior['mode'])


def main():
    global CERTIFICATE, TEAM, APP, BIN_DIR, IDENTIFIER
    parser = argparse.ArgumentParser(description='Install macOS tools as a fixed signed app without restarting services.')
    parser.add_argument('--from', dest='source', type=Path)
    parser.add_argument('--home', type=Path, default=os.environ.get('ST_MACOS_INSTALL_HOME'),
                        help='installation home for isolated staging/tests (default: user home)')
    parser.add_argument('--bin-dir', default=BIN_DIR)
    parser.add_argument('--app', default=APP)
    parser.add_argument('--identity', default=CERTIFICATE)
    parser.add_argument('--team-id', default=TEAM)
    parser.add_argument('--identifier', default=IDENTIFIER)
    parser.add_argument('--job', type=Path)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument('--prepare-only', action='store_true')
    modes.add_argument('--install-app', type=Path)
    modes.add_argument('--verify-app', type=Path)
    modes.add_argument('--backup-only', action='store_true')
    modes.add_argument('--restore-app', action='store_true')
    args = parser.parse_args()
    CERTIFICATE, TEAM, APP, BIN_DIR, IDENTIFIER = args.identity or None, args.team_id or None, args.app, args.bin_dir, args.identifier
    home = args.home.expanduser().absolute() if args.home is not None else Path.home()
    lock_root = home / '.local/state/st3/macos-installs'
    lock_root.mkdir(parents=True, exist_ok=True)
    with open(lock_root / (hashlib.sha256(str(fixed(home)).encode()).hexdigest() + '.lock'), 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        verifying_artifact = args.verify_app or args.install_app or args.backup_only or args.restore_app
        if verifying_artifact and CERTIFICATE and re.fullmatch(r'[a-fA-F0-9]{40}', CERTIFICATE):
            # A staged signed bundle carries its certificate. Verification/install
            # needs no signing private key on the receiving machine.
            CERTIFICATE = CERTIFICATE.upper()
        else:
            resolve_identity()
        if args.verify_app:
            print(json.dumps(verify(args.verify_app), sort_keys=True))
            return
        if args.job is None:
            if args.prepare_only or args.backup_only or args.restore_app:
                parser.error('--job is required for preparing, backing up or restoring a transaction')
            root = home / '.local/state/st3/macos-installs'
            root.mkdir(parents=True, exist_ok=True)
            args.job = Path(tempfile.mkdtemp(prefix='install-', dir=root))
        args.job.mkdir(parents=True, exist_ok=True)
        if args.backup_only:
            had_app = backup(home, args.job)
            write_atomic(args.job / 'previous.json', json.dumps({'had_app': had_app}).encode())
            print(json.dumps({'had_app': had_app}))
            return
        if args.restore_app:
            prior = json.loads((args.job / 'previous.json').read_text())
            restore(home, args.job, prior['had_app'])
            return
        if args.install_app:
            identity = verify(args.install_app)
            install(args.install_app, home, args.job)
            print(json.dumps(identity, sort_keys=True))
            return
        if args.source is None:
            parser.error('--from is required')
        built = {name: args.source / name for name in ['st3', 'stui']}
        for path in built.values():
            if not path.is_file() or not os.access(path, os.X_OK):
                parser.error('missing executable: ' + str(path))
        app, identity, payload = prepare(home, args.job, built)
        if args.prepare_only:
            print(json.dumps({'app': str(app), 'identity': identity, 'payload_hashes': payload}, sort_keys=True))
            return
        had_app = backup(home, args.job)
        write_atomic(args.job / 'previous.json', json.dumps({'had_app': had_app}).encode())
        try:
            install(app, home, args.job)
        except Exception:
            restore(home, args.job, had_app)
            raise
        print(json.dumps({'app': str(fixed(home)), 'identity': identity, 'payload_hashes': payload}, sort_keys=True))


if __name__ == '__main__':
    try:
        main()
    except (RuntimeError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit('install-macos: ' + str(error))

