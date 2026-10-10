#!/usr/bin/env python3
"""One arm, entered only by the hosted outer controller inside its ordinary devshell."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import sys
import time
import tempfile

spec = importlib.util.spec_from_file_location('diet', Path(__file__).with_name('ci-log-diet-common.py'))
c = importlib.util.module_from_spec(spec); spec.loader.exec_module(c)


def artifact(raw, metadata, source):
    source = Path(source).resolve()
    manifest = source / 'crates/st3/Cargo.toml'
    integration = source / 'crates/st3/tests/integration.rs'
    if metadata.get('version') != 1 or 'resolve' not in metadata or metadata['resolve'] is not None:
        raise ValueError('not manifest-only metadata with null resolve')
    if Path(metadata['workspace_root']).resolve() != source:
        raise ValueError('metadata workspace differs from actual arm source')
    packages = [p for p in metadata['packages'] if p['name'] == 'st3']
    if len(packages) != 1: raise ValueError('not one actual workspace st3 package')
    package = packages[0]
    if package['id'] not in metadata['workspace_members'] or Path(package['manifest_path']).resolve() != manifest:
        raise ValueError('st3 package or manifest outside actual workspace')
    targets = [t for t in package['targets'] if t['name'] == 'integration']
    if (len(targets) != 1 or targets[0]['kind'] != ['test'] or targets[0].get('test') is not True
            or Path(targets[0]['src_path']).resolve() != integration):
        raise ValueError('not one manifest integration test target in actual source')
    rows = []
    for line in raw.splitlines():
        obj = json.loads(line)
        if (obj.get('reason') == 'compiler-artifact' and obj.get('package_id') == package['id']
                and obj.get('target', {}).get('name') == 'integration'):
            rows.append(obj)
    if (len(rows) != 1 or rows[0]['target'].get('kind') != ['test']
            or Path(rows[0]['target']['src_path']).resolve() != integration
            or rows[0].get('profile', {}).get('test') is not True or not rows[0].get('executable')
            or 'test-support' not in rows[0].get('features', [])):
        raise ValueError('not one compiled integration test artifact with emitted test-support')
    return rows[0], package


def binary_path(row, metadata, source):
    binary = Path(row['executable']).resolve(strict=True)
    target = Path(metadata['target_directory']).resolve(strict=True)
    if not binary.is_relative_to(target) or not binary.is_relative_to(Path(source).resolve() / 'target'):
        raise ValueError('binary outside actual metadata and arm source target trees')
    return binary


def run_arm(args, runner_factory=c.Runner, clock=time.monotonic):
    root = Path(args.root); source = Path(args.source); plan = c.load(args.plan)
    record = c.load(args.deadline); window = c.load(args.window)
    private = None
    result = {'status': 'NOT_QUALIFIED', 'initial': 'UNKNOWN', 'commands': [], 'arm': plan['name']}
    runner = runner_factory(root, clock)
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP): signal.signal(sig, runner.interrupt)
    try:
        c.validate_arm(window, record, c.hosted(), clock())
        # Nix may set PATH and normal tool configuration. Owner/CI caller never changes.
        if c.identity() != c.load(args.caller): raise ValueError('admission owner/CI identity changed')
        for file in plan['files']:
            expected = plan['overlay_file']['overlay_file_sha256'] if file['path'] == 'crates/st3/tests/log_diet.rs' else file['sha256']
            if c.sha(source / file['path']) != expected: raise ValueError('post-devshell source changed')
        paths = {k: shutil.which(k) for k in ('cargo', 'rustc', 'nix', 'timeout', 'python3')}
        if not all(paths.values()): raise ValueError('missing actual ordinary tools')
        env = {k: os.environ.get(k) for k in c.GOV_KEYS}
        env['DBUS_SESSION_BUS_ADDRESS'] = c.sha_value(env['DBUS_SESSION_BUS_ADDRESS'])
        c.write_json(root / 'admission.json', {'caller': c.identity(), 'governance': env,
            'paths': paths, 'path_hashes': {k: c.sha(v) for k, v in paths.items()},
            'pid': os.getpid(), 'proc_stat': Path('/proc/self/stat').read_text(),
            'cgroup': Path('/proc/self/cgroup').read_text(),
            'guardian_child_identity': 'Original pinned guardian clean re-execs ST_AGENT/ST3_SUBJECT only AFTER Cargo admission; retains original caller pidfds',
            'fixture_binding': 'same-process Client::Unix / serve_unix bind_ancestry=false (source witness, not runtime certification)'})
        for tool, extra in [('cargo', ['--version','--verbose']), ('rustc', ['--version','--verbose']), ('nix',['--version'])]:
            runner.checked('tool-' + tool, [paths[tool], *extra], source, window['active'], window['end'])
        c.write_json(root / 'source.json', plan)
        runner.checked('build', [paths['cargo'], 'test', '-p', 'st3', '--features', 'test-support',
                       '--test', 'integration', '--locked', '--no-run', '--message-format=json'],
                       source, window['active'], window['end'])
        runner.checked('manifest-metadata', [paths['cargo'], 'metadata', '--no-deps', '--locked', '--offline', '--format-version', '1',
                       '--features', 'st3/test-support'], source, window['active'], window['end'])
        metadata = c.load(root / 'manifest-metadata.stdout')
        row, package = artifact(c.text(root / 'build.stdout'), metadata, source)
        binary = binary_path(row, metadata, source)
        c.write_json(root / 'binary.json', {'compiler_artifact': row, 'manifest_package': package,
                                           'metadata_scope': 'Actual cargo metadata --no-deps --locked --offline --format-version 1 --features st3/test-support: workspace manifest description only',
                                           'dependency_resolution': 'NOT_REQUESTED', 'metadata_resolve': None,
                                           'compiled_target_features': row['features'],
                                           'path': str(binary), 'sha256': c.sha(binary)})
        runner.checked('inventory', [str(binary), '--exact', c.CASE, '--list', '--format', 'terse'],
                       source, window['active'], window['end'])
        runner.checked('inventory-ignored', [str(binary), '--exact', c.CASE, '--list', '--ignored', '--format', 'terse'],
                       source, window['active'], window['end'])
        c.inventory(c.text(root / 'inventory.stdout'), c.text(root / 'inventory-ignored.stdout'))
        c.full_case(window, clock())  # Never reduce 180 to fit a depleted arm budget.
        # Short guardian-owned roots preserve the original Unix socket path budget.
        private = Path(tempfile.mkdtemp(prefix='diet-', dir='/tmp'))
        c.write_json(root / 'private-parent.json', {'path':str(private), 'fresh':True})
        guardian = source / 'scripts/st3_test_process.py'
        if c.sha(guardian) != '516bd69b22eedcd006925070ab5011c9c270e89451a77473940a592369d22acc':
            raise ValueError('guardian source mismatch')
        argv = [sys.executable, str(guardian), '--owner', str(os.getpid()),
                '--temporary-root', 'HOME', 'log-diet-home-', str(private),
                '--temporary-root', 'TMPDIR', 'log-diet-tmp-', str(private), '--keep', '--',
                paths['timeout'], '--signal=TERM', '--kill-after=10s', '180s', str(binary),
                '--exact', c.CASE, '--nocapture', '--test-threads', '1']
        # Outer deadline additionally bounds guardian completion/cleanup. Raw fd order is retained.
        row = runner.run('case', argv, source, min(window['active'], clock() + 190), window['end'], merged=True)
        observed = c.result(row['exit'], c.text(root / 'case.stdout'), row['cleanup_confirmed'],
                            row['timed_out'] or row['cancelled'] or row['exit'] == 124,
                            row['output_complete'])
        result.update(observed)
        result['status'] = 'COMPLETE' if observed['measurement_complete'] else 'NOT_QUALIFIED'
    except BaseException as exc:
        result['status'] = 'NOT_QUALIFIED'
        result['refusal_type'] = type(exc).__name__  # No raw environment/errors/secrets in summary.
    finally:
        result['commands'] = runner.commands
        result['cleanup_confirmed'] = c.cleanup(window['end'])
        if not result['cleanup_confirmed']:
            result['status'] = 'NOT_QUALIFIED'
        elif private is not None:
            try:
                if any(p.is_symlink() for p in private.rglob('*')):
                    raise ValueError('private retention cannot follow links')
                if clock() >= window['end']: raise ValueError('retention deadline expired')
                shutil.move(str(private), str(root / 'private'))
                if clock() >= window['end']: raise ValueError('retention exceeded cleanup budget')
            except BaseException as exc:
                result['status'] = 'NOT_QUALIFIED'
                result['retention_refusal_type'] = type(exc).__name__
        c.write_json(root / 'arm-terminal.json', result)
    return 0 if result['status'] == 'COMPLETE' else 1


def main():
    p = argparse.ArgumentParser(); p.add_argument('--root', required=True); p.add_argument('--source', required=True)
    p.add_argument('--plan', required=True); p.add_argument('--deadline', required=True)
    p.add_argument('--window', required=True); p.add_argument('--caller', required=True)
    args = p.parse_args(); c.subreaper()
    # Runner cancellation is set at the caller level; do not turn cancellation into an arm retry.
    return run_arm(args)


if __name__ == '__main__': raise SystemExit(main())
