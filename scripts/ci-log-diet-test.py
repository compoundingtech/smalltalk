#!/usr/bin/env python3
"""Pure/synthetic caller controls only. No candidate, Rust, Nix or native invocation."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, Mock


def module(name, file):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(file))
    m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m); return m

c = module('common', 'ci-log-diet-common.py')
s = module('study', 'ci-log-diet-study.py')
a = module('arm', 'ci-log-diet-arm.py')
IDENTITY = {k: None for k in c.CI_KEYS}
IDENTITY.update(GITHUB_ACTIONS='true', GITHUB_EVENT_NAME='workflow_dispatch', GITHUB_RUN_ID='mock-run',
                GITHUB_RUN_ATTEMPT='1', GITHUB_JOB='mock-job', RUNNER_NAME='mock-runner')
ASSIGNMENT = 'doc/fleet/smalltalk/speed/2026-10-10/log-diet-three-source-execution-assignment@' + 'a' * 64


def witness(index, availability=None):
    body = {'run_subject': {'value': 'mission-run/private', 'original_utf8_bytes': 19, 'truncated': False}, 'run': 'absent'}
    if availability: body['availability'] = availability
    return c.WITNESS + json.dumps({'boundary': 'observer-unchanged-at-original-60s-deadline',
              'sampling': c.SAMPLING, 'root_loop_index': index, 'witness': body}) + '\n'


def raw(success=True, interleaved=True, diagnostic=None):
    middle = 'fixture summary\n| op | wall | SQL |\n' if interleaved else ''
    if diagnostic is not None: middle += diagnostic
    return ('running 1 test\ntest ' + c.CASE + ' ... ' + middle + ('ok' if success else 'FAILED') + '\n\n' +
            ('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 99 filtered out; finished in 1.00s\n'
             if success else 'test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 99 filtered out; finished in 1.00s\n'))


class Control(unittest.TestCase):
    def record(self, start=9):
        return {'identity': IDENTITY, 'start': start, 'cutoff': start + 6000,
                'setup_end': start + 600, 'upload_end': start + 6900, 'boot_sha256': c.host_binding()}

    def test_interleaved_success_and_immediate_success(self):
        for interleaved in (True, False):
            r = c.result(0, raw(interleaved=interleaved), True)
            self.assertEqual((r['initial'], r['continue']), ('PASS', True))

    def test_interleaved_failure_actual_witness(self):
        r = c.result(101, raw(False, diagnostic=witness(0) + witness(1)), True)
        self.assertEqual((r['initial'], r['continue']), ('FAIL', True))

    def test_missing_witness_preserves_failure_and_stops(self):
        r = c.result(101, raw(False), True)
        self.assertEqual((r['initial'], r['continue']), ('FAIL', False))

    def test_malformed_unavailable_truncated_readerror_refuse(self):
        for diagnostic in (c.WITNESS + '{bad\n', c.WITNESS + 'diagnostic unavailable; original failure preserved\n',
                witness(0) + witness(1, 'unavailable'), (witness(0) + witness(1)).replace('"truncated": false', '"truncated": true'),
                witness(0) + witness(1).replace('"run": "absent"', '"read_error": "error"')):
            r = c.result(101, raw(False, diagnostic=diagnostic), True)
            self.assertEqual((r['initial'], r['continue']), ('FAIL', False))

    def test_partial_duplicate_wrong_sampling_witness_refuse(self):
        for diagnostic in (witness(0), witness(0) * 2, witness(0) + witness(1) + witness(1),
                           (witness(0) + witness(1)).replace(c.SAMPLING, 'invented cut'),
                           (witness(0) + witness(1)).replace('original_utf8_bytes": 19', 'original_utf8_bytes": 20')):
            self.assertFalse(c.result(101, raw(False, diagnostic=diagnostic), True)['continue'])

    def test_raw_missing_duplicate_extra_wrong_exit_refuse(self):
        for code, text in [(0, ''), (0, raw() * 2), (0, raw() + 'test other ... ok\n'),
                           (101, raw()), (0, raw(False)), (0, raw().replace('1 passed', '0 passed'))]:
            self.assertEqual(c.result(code, text, True)['initial'], 'UNKNOWN')

    def test_cleanup_timeout_output_loss_no_success(self):
        for clean, timeout, complete in [(False, False, True), (True, True, True), (True, False, False)]:
            r = c.result(0, raw(), clean, timeout, complete)
            self.assertEqual((r['initial'], r['continue']), ('PASS', False))

    def test_actual_inventory_one_nonignored(self):
        c.inventory(c.CASE + ': test\n')
        with self.assertRaises(ValueError): c.inventory(c.CASE + ': test\n', c.CASE + ': test\n')
        for value in ('', c.CASE + ': benchmark\n', c.CASE + ': test\n' * 2, 'other: test\n'):
            with self.assertRaises(ValueError): c.inventory(value)

    def test_fresh_old_clock_future_reset_crossrun(self):
        for start in (0, 9, 10000000):
            r = self.record(start)
            self.assertEqual(c.deadline(r, IDENTITY, start), start + 6000)
            for bad in (start - 1, start + 6000):
                with self.assertRaises(ValueError): c.deadline(r, IDENTITY, bad)
            with self.assertRaises(ValueError): c.deadline(r, dict(IDENTITY, GITHUB_RUN_ID='other'), start)
            r['cutoff'] += 1
            with self.assertRaises(ValueError): c.deadline(r, IDENTITY, start)

    def test_full180_no_shortening_or_arm_reset(self):
        r = self.record(0); w = c.arm_window(r, IDENTITY, 600)
        self.assertEqual(c.full_case(w, 600), 780)
        with self.assertRaises(ValueError): c.full_case(w, w['active'] - 179)
        with self.assertRaises(ValueError): c.validate_arm(dict(w, end=w['end'] + 1), r, IDENTITY, 610)
        with self.assertRaises(ValueError): c.arm_window(r, IDENTITY, 5801)

    def caller(self, delay=0, fail=None, cleanup=True, setup_time=10):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); record = self.record(0); c.write_json(root/'deadline.json', record)
            now = [setup_time]; commands = []; windows = []
            plans = [{'name': name, 'commit': source, 'effective_overlay_tree': 'mock-tree'}
                     for name, source in zip(('main','32895','af02'), c.SOURCES)]
            class FakeRunner:
                def __init__(self, output, clock): self.root = Path(output); self.commands = commands
                def interrupt(self, *args): pass
                def checked(self, label, argv, cwd, active, end):
                    if now[0] >= active: raise ValueError('mock expired before command')
                    commands.append(label)
                    value = '\n'.join(p['commit'] + ' refs/heads/frozen/' + p['name'] for p in plans) if label == 'bundle-heads' else 'mock-identity\n'
                    (self.root/(label+'.stdout')).write_text(value)
                    return {'exit': 0}
                def run(self, label, argv, cwd, active, end):
                    # This mock simulates startup BEFORE the arm gets control.
                    arm = root/label; w = c.load(arm/'window.json'); windows.append(dict(w))
                    now[0] += delay
                    c.validate_arm(w, record, IDENTITY, now[0])
                    commands.append(label)
                    value = {'status':'COMPLETE', 'initial':'PASS', 'continue': True, 'cleanup_confirmed':cleanup}
                    if fail: value.update({'initial':'FAIL', 'status':'NOT_QUALIFIED', 'continue':False})
                    c.write_json(arm/'arm-terminal.json', value)
                    return {'exit':0, 'timed_out':False, 'cancelled':False, 'output_complete':True, 'cleanup_confirmed':cleanup}
            args = argparse.Namespace(repo=tmp, root=tmp, deadline=str(root/'deadline.json'), assignment=ASSIGNMENT)
            with patch.object(s.c, 'hosted', return_value=IDENTITY), patch.object(s.c, 'identity', return_value=IDENTITY), \
                    patch.object(s.c, 'cleanup', return_value=cleanup), patch.object(s.signal, 'signal'):
                code = s.study(args, FakeRunner, lambda: now[0], prepare=lambda *x: None,
                               verify=lambda repo:(root, plans, {'files':[]}))
            return code, c.load(root/'terminal.json'), commands, windows

    def test_production_caller_three_order_no_retry(self):
        code, report, commands, windows = self.caller()
        self.assertEqual(code, 0)
        self.assertEqual([r['source'] for r in report['arms']], list(c.SOURCES))
        self.assertEqual(commands[-3:], ['arm-main','arm-32895','arm-af02'])
        self.assertEqual(len(windows), 3)

    def test_caller_slow_devshell_consumes_existing_arm(self):
        code, report, commands, windows = self.caller(delay=1681)
        self.assertEqual(code, 1); self.assertEqual(len(windows), 1)
        self.assertEqual(windows[0]['start'], 10)
        self.assertEqual(windows[0]['active'], 1690)
        self.assertEqual(report['status'], 'NOT_QUALIFIED')

    def test_caller_incomplete_failure_stops_original_failure_retained(self):
        code, report, commands, windows = self.caller(fail=True)
        self.assertEqual(code, 1); self.assertEqual(len(windows), 1)
        self.assertTrue(report['initial_failure_preserved'])

    def test_caller_indeterminate_cleanup_stops(self):
        code, report, commands, windows = self.caller(cleanup=False)
        self.assertEqual(code, 1); self.assertEqual(len(windows), 1)
        self.assertFalse(report['cleanup_confirmed'])

    def test_caller_setup_expired_starts_no_arm(self):
        code, report, commands, windows = self.caller(setup_time=601)
        self.assertEqual(code, 1); self.assertEqual(windows, [])

    def arm_caller(self, case_raw, code=101, clean=True, delay=0):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); source = root / 'source'; source.mkdir()
            (source / 'scripts').mkdir()
            # Ordinary source bytes only. Never execute this pinned guardian or mock binary.
            guardian = Path(__file__).resolve().parents[1] / 'data/log-diet-three-source/guardian.py'
            (source / 'scripts/st3_test_process.py').write_bytes(guardian.read_bytes())
            binary = source / 'mock-binary'; binary.write_text('synthetic data; never executable')
            now = [10]; record = self.record(0); window = c.arm_window(record, IDENTITY, 10)
            for name, value in [('deadline',record),('window',window),('caller',IDENTITY),('plan',{'name':'mock','files':[]})]:
                c.write_json(root/(name+'.json'), value)
            class FakeRunner:
                def __init__(self, output, clock): self.commands = []; self.root = Path(output)
                def interrupt(self, *args): pass
                def checked(self, label, argv, cwd, active, end):
                    self.commands.append({'label':label, 'argv':argv})
                    value = 'synthetic tool receipt'
                    if label == 'build':
                        value = json.dumps({'reason':'compiler-artifact','package_id':'mock-st3',
                            'target':{'name':'integration','kind':['test']},'profile':{'test':True},
                            'features':['test-support'],'executable':str(binary)})
                        now[0] += delay
                    if label == 'features':
                        value = json.dumps({'packages':[{'name':'st3','id':'mock-st3'}],
                            'resolve':{'nodes':[{'id':'mock-st3','features':['test-support']}]},
                            'target_directory':str(source)})
                    if label == 'inventory': value = c.CASE + ': test\n'
                    if label == 'inventory-ignored': value = ''
                    (self.root/(label+'.stdout')).write_text(value)
                    return {'exit':0}
                def run(self, label, argv, cwd, active, end, merged=False):
                    self.commands.append({'label':label, 'argv':argv})
                    (self.root/(label+'.stdout')).write_text(case_raw)
                    return {'exit':code,'cleanup_confirmed':clean,'timed_out':False,
                            'cancelled':False,'output_complete':True}
            args = argparse.Namespace(root=tmp, source=str(source), plan=str(root/'plan.json'),
                    deadline=str(root/'deadline.json'),window=str(root/'window.json'),caller=str(root/'caller.json'))
            with patch.object(a.c,'hosted',return_value=IDENTITY), patch.object(a.c,'identity',return_value=IDENTITY), \
                    patch.object(a.c,'cleanup',return_value=clean), patch.object(a.signal,'signal'), \
                    patch.object(a.shutil,'which',return_value=str(binary)):
                status = a.run_arm(args, FakeRunner, lambda:now[0])
            return status, c.load(root/'arm-terminal.json')

    def test_actual_arm_caller_interleaved_success_and_failure(self):
        for value, code, initial in [(raw(),0,'PASS'),(raw(False,diagnostic=witness(0)+witness(1)),101,'FAIL')]:
            status, report = self.arm_caller(value, code)
            self.assertEqual(status,0); self.assertEqual(report['initial'],initial)
            argv = report['commands'][-1]['argv']
            self.assertIn('180s',argv); self.assertEqual(argv.count(c.CASE),1)
            self.assertEqual(argv[-3:],['--nocapture','--test-threads','1'])

    def test_actual_arm_caller_failure_diagnostics_loss_stops(self):
        for diagnostic in ('',c.WITNESS+'{bad\n',witness(0),witness(0)+witness(1,'unavailable'),
                           (witness(0)+witness(1)).replace('"truncated": false','"truncated": true')):
            status, report = self.arm_caller(raw(False,diagnostic=diagnostic))
            self.assertEqual(status,1); self.assertEqual(report['initial'],'FAIL')
            self.assertFalse(report['continue'])

    def test_actual_arm_caller_slow_build_refuses_shortened_case(self):
        status, report = self.arm_caller(raw(),0,delay=1501)
        self.assertEqual(status,1)
        self.assertNotIn('case',[row['label'] for row in report['commands']])

    def test_exact_compiled_artifact_feature_inventory_refusals(self):
        m = {'packages':[{'name':'st3','id':'st3-actual'}], 'resolve':{'nodes':[{'id':'st3-actual','features':['test-support']}]}}
        row = {'reason':'compiler-artifact','package_id':'st3-actual', 'target':{'name':'integration','kind':['test']},
               'profile':{'test':True},'features':['test-support'],'executable':'mock-binary'}
        self.assertEqual(a.artifact(json.dumps(row), m)[0], row)
        for lines in ('', json.dumps(row)+'\n'+json.dumps(row), json.dumps(dict(row, features=[]))):
            with self.assertRaises(ValueError): a.artifact(lines, m)

    def test_upload_expired_crossrun_and_negative_artifact(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp); record = self.record(0); c.write_json(root/'deadline.json', record)
            args = argparse.Namespace(root=tmp, deadline=str(root/'deadline.json'))
            with patch.object(s.c,'hosted',return_value=IDENTITY), patch.object(s.time,'monotonic',return_value=6900):
                with self.assertRaises(ValueError): s.retain(args)
            with patch.object(s.c,'hosted',return_value=dict(IDENTITY,GITHUB_RUN_ID='other')), patch.object(s.time,'monotonic',return_value=100):
                with self.assertRaises(ValueError): s.retain(args)
            with patch.object(s.c,'hosted',return_value=IDENTITY), patch.object(s.time,'monotonic',return_value=100): s.retain(args)
            self.assertEqual(c.load(root/'artifact-manifest.json')['terminal_status'], 'NOT_QUALIFIED')

    def test_malformed_and_crosshost_deadline_refuse(self):
        for change in ({'start':True},{'cutoff':'6000'},{'upload_end':float('inf')},{'boot_sha256':'other-host'}):
            with self.assertRaises(ValueError): c.deadline(dict(self.record(0),**change),IDENTITY,10)

    def test_explicit_actionlint_from_actual_pinned_wrapper(self):
        lint = module('lint','ci-log-diet-actionlint.py')
        wrapper = "export GENIE_ACTIONLINT_BIN='/nix/store/mock-actionlint/bin/actionlint'\n"
        self.assertEqual(lint.resolve(wrapper),'/nix/store/mock-actionlint/bin/actionlint')
        for value in ('',wrapper+wrapper,wrapper.replace('/nix/store/','/uncontrolled/')):
            with self.assertRaises(ValueError): lint.resolve(value)

    def test_pinned_guardian_doublefork_pidfds_and_owned_cleanup(self):
        import ast
        path = Path(__file__).resolve().parents[1]/'data/log-diet-three-source/guardian.py'
        self.assertEqual(c.sha(path),'516bd69b22eedcd006925070ab5011c9c270e89451a77473940a592369d22acc')
        source = path.read_text(); parsed = ast.parse(source)
        f = next(n for n in parsed.body if isinstance(n,ast.FunctionDef) and n.name=='run_supervised')
        segment = ast.get_source_segment(source,f)
        self.assertEqual(segment.count('os.fork()'),2)
        self.assertLess(segment.index('os.pidfd_open(pid)'),segment.index('os.fork()'))
        self.assertIn('os.setsid()',segment)
        spec = importlib.util.spec_from_file_location('guardian_pure',path)
        g = importlib.util.module_from_spec(spec);spec.loader.exec_module(g)
        events=[];task=Mock(pid=222);task.wait.side_effect=lambda:events.append('wait') or 143
        fd=Mock();fd.fileno.return_value=333;context=Mock()
        context.__enter__=Mock(return_value=fd);context.__exit__=Mock(return_value=False)
        poller=Mock();poller.poll.return_value=[(111,1)];lib=Mock();lib.prctl.return_value=0
        with patch.object(g.ctypes,'CDLL',return_value=lib),patch.object(g.signal,'signal'), \
             patch.object(g.subprocess,'Popen',return_value=task),patch.object(g.os,'pidfd_open',return_value=444), \
             patch.object(g.os,'fdopen',return_value=context),patch.object(g.select,'poll',return_value=poller), \
             patch.object(g,'kill_group',side_effect=lambda pid:events.append(('owned-kill',pid))), \
             patch.object(g,'reap_descendants',side_effect=lambda:events.append('reap')), \
             patch.object(g.os,'write',side_effect=lambda fd,b:events.append(('terminal',b))):
            g.guardian(['mock-only'],[111],555)
        self.assertEqual(events,[('owned-kill',222),'wait','reap',('terminal',b'143')])
        lib.prctl.assert_called_once_with(36,1,0,0,0)

    def test_command_timeout_cancel_signals_owned_group_and_reaps(self):
        with tempfile.TemporaryDirectory() as tmp:
            runner = c.Runner(tmp, clock=lambda: 100)
            proc = Mock(pid=4242); proc.poll.return_value = None; proc.wait.return_value = 143
            with patch.object(c.subprocess,'Popen',return_value=proc), patch.object(c.os,'pidfd_open',return_value=999), \
                    patch.object(c.os,'close'), patch.object(c.os,'killpg') as kill, patch.object(c,'cleanup',return_value=True):
                # Already cancelled must refuse before Popen; no unrelated action.
                runner.cancelled = True
                with self.assertRaises(ValueError): runner.run('never', ['mock-only'], tmp, 200, 320)
                runner.cancelled = False
                seq = iter((100,100,201,201,201,201)); runner.clock = lambda: next(seq,201)
                r = runner.run('timeout',['mock-only'],tmp,200,320)
                self.assertTrue(r['timed_out']); kill.assert_called_once_with(4242, c.signal.SIGTERM)


if __name__ == '__main__': unittest.main()
