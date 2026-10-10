#!/usr/bin/env python3
"""Bounded hosted diagnostic plumbing; importing this module starts no process."""
import ctypes
import hashlib
import json
import math
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import time

CASE = 'log_diet::a_busy_daemon_replicates_at_most_a_fifth_of_what_main_would'
SOURCES = ('5e4c45196413a44d85ebe2807dfcca15cd1144f5',
           '32895fd5f7571ac98b5e4814bab6fced6daf3c6b',
           'af02c83f923abf27e21ec56757e7c928677dcac2')
CI_KEYS = ('ST_AGENT', 'ST3_SUBJECT', 'GITHUB_ACTIONS', 'GITHUB_EVENT_NAME',
           'GITHUB_RUN_ID', 'GITHUB_RUN_ATTEMPT', 'GITHUB_JOB', 'RUNNER_NAME')
GOV_KEYS = ('CARGO_SLOT_HELD', 'CARGO_REAL', 'CARGO_PRIORITY_FILE',
            'CARGO_HEAVY_SLOTS', 'CARGO_SLOTS', 'DBUS_SESSION_BUS_ADDRESS', 'XDG_RUNTIME_DIR')
LIMIT = 64 * 1024 * 1024
WITNESS = 'st3-log-diet-witness '
SAMPLING = 'sequential private-store reads after failure, not the pass cut'
BOUNDARIES = {'reconcile-error-or-join-panic', 'observer-unchanged-at-original-60s-deadline'}


def sha_value(value):
    return hashlib.sha256(value.encode()).hexdigest() if value is not None else None


def sha(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def write_json(path, value):
    # Each phase receipt is fresh. A duplicate invocation refuses rather than overwrites.
    with open(path, 'x', encoding='utf-8') as f:
        json.dump(value, f, indent=2, sort_keys=True); f.write('\n')


def load(path):
    return json.loads(Path(path).read_text())


def text(path):
    p = Path(path)
    if p.stat().st_size > LIMIT:
        raise ValueError('retained output exceeds completeness limit')
    return p.read_text(encoding='utf-8', errors='strict')


def identity(env=os.environ):
    return {k: env.get(k) for k in CI_KEYS}


def hosted(env=os.environ):
    i = identity(env)
    if i['GITHUB_ACTIONS'] != 'true' or i['GITHUB_EVENT_NAME'] != 'workflow_dispatch':
        raise ValueError('hosted manual invocation required')
    if not all(i[k] for k in ('GITHUB_RUN_ID', 'GITHUB_RUN_ATTEMPT', 'GITHUB_JOB', 'RUNNER_NAME')):
        raise ValueError('missing hosted identity')
    return i


def host_binding():
    return sha_value(Path('/proc/sys/kernel/random/boot_id').read_text())


def deadline(record, expected, now):
    if record.get('identity') != expected or record.get('boot_sha256') != host_binding():
        raise ValueError('deadline identity mismatch')
    for key in ('start', 'cutoff', 'setup_end', 'upload_end'):
        value = record.get(key)
        if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
            raise ValueError('malformed deadline')
    start = record['start']
    if (record['cutoff'], record['setup_end'], record['upload_end']) != (start + 6000, start + 600, start + 6900):
        raise ValueError('changed or reset deadline')
    if not math.isfinite(now) or now < start or now >= record['cutoff']:
        raise ValueError('future or expired deadline')
    return record['cutoff']


def arm_window(record, expected, now):
    cutoff = deadline(record, expected, now)
    end = min(now + 1800, cutoff)
    if end - now < 300:
        raise ValueError('cannot reserve full180 plus cleanup120')
    return {'start': now, 'end': end, 'active': end - 120}


def validate_arm(window, record, expected, now):
    cutoff = deadline(record, expected, now)
    start = window['start']
    if not math.isfinite(start) or start < record['start'] or start > now:
        raise ValueError('invalid arm start')
    if window['end'] != min(start + 1800, cutoff) or window['active'] != window['end'] - 120:
        raise ValueError('reset arm deadline')
    if now >= window['active']:
        raise ValueError('arm active deadline expired')


def full_case(window, now):
    if window['active'] - now < 180:
        raise ValueError('no full180 available; case refused, never shortened')
    return now + 180


def inventory(raw, ignored=""):
    if ignored.strip(): raise ValueError("selected name is ignored or ignored inventory malformed")
    if [x.strip() for x in raw.splitlines() if x.strip()] != [CASE + ': test']:
        raise ValueError('exact one nonignored full name required')


def no_loss(value):
    if isinstance(value, dict):
        if value.get('availability') == 'unavailable' or 'read_error' in value or value.get('truncated') is True:
            return False
        if 'value' in value:
            if not isinstance(value['value'], str) or value.get('truncated') is not False:
                return False
            if value.get('original_utf8_bytes') != len(value['value'].encode('utf-8')):
                return False
        return all(no_loss(v) for v in value.values())
    if isinstance(value, list):
        return all(no_loss(v) for v in value)
    return True


def witness_complete(raw):
    rows = []
    for line in raw.splitlines():
        # A harness prefix can precede the first diagnostic on the same line.
        if WITNESS in line:
            if line.count(WITNESS) != 1:
                return False
            payload = line.split(WITNESS, 1)[1]
            try:
                row = json.loads(payload)
            except (ValueError, TypeError):
                return False
            if not isinstance(row, dict) or set(row) != {'boundary', 'sampling', 'root_loop_index', 'witness'}:
                return False
            if row['boundary'] not in BOUNDARIES or row['sampling'] != SAMPLING:
                return False
            if type(row['root_loop_index']) is not int or not isinstance(row['witness'], dict) or not row['witness']:
                return False
            if not no_loss(row['witness']):
                return False
            # Source-recorded absent runs/steps/faults are facts, not transport loss.
            w = row['witness']
            if 'run_subject' not in w:
                return False
            if 'run' in w:
                if w['run'] != 'absent': return False
            elif 'step' in w:
                if w['step'] != 'missing': return False
            elif not {'run_status', 'run_phase', 'step_subject', 'step_status', 'step_reason',
                      'claim_expires_at_unix_ms', 'execution_started_at_unix_ms', 'timeout_ms',
                      'timeout_extension_ms', 'latest_reconcile_fault'} <= set(w):
                return False
            rows.append(row)
    return (len(rows) == 2 and sorted(r['root_loop_index'] for r in rows) == [0, 1]
            and len({r['boundary'] for r in rows}) == 1)


def result(code, raw, clean, timed_out=False, complete=True):
    value = {'initial': 'UNKNOWN', 'measurement_complete': False, 'continue': False}
    # libtest's prefix and result can surround arbitrarily interleaved fixture lines.
    prefix = 'test ' + CASE + ' ... '
    if raw.count(prefix) != 1 or len(re.findall(r'^running 1 test\s*$', raw, re.M)) != 1:
        return value
    if len(re.findall(r'^test (?!result:)', raw, re.M)) != 1:
        return value
    tail = raw.split(prefix, 1)[1]
    endings = re.findall(r'^(ok|FAILED)\s*$', tail, re.M)
    # Immediate result on the same prefix line is also an anchored tail line.
    summaries = re.findall(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out; finished in ([0-9.]+)s\s*$', raw, re.M)
    if len(endings) != 1 or len(summaries) != 1:
        return value
    s = summaries[0]
    if s[:5] == ('ok', '1', '0', '0', '0') and code == 0 and endings == ['ok']:
        value['initial'] = 'PASS'
    elif s[:5] == ('FAILED', '0', '1', '0', '0') and code == 101 and endings == ['FAILED']:
        value['initial'] = 'FAIL'
    else:
        return value
    diagnostic = value['initial'] == 'PASS' or witness_complete(raw)
    value['measurement_complete'] = clean and not timed_out and complete and diagnostic
    value['continue'] = value['measurement_complete']
    return value


def subreaper():
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(36, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), 'subreaper failed')


def child_pids():
    # Only this controller's direct children, including adopted descendants.
    return [int(p) for p in Path(f'/proc/{os.getpid()}/task/{os.getpid()}/children').read_text().split()]


def cleanup(until):
    while time.monotonic() < until:
        pids = child_pids()
        if not pids:
            return True
        for pid in pids:
            try:
                fd = os.pidfd_open(pid)
                try:
                    # An unreaped owned child reserves its PID; never a fleet census.
                    try: os.killpg(pid, signal.SIGKILL)
                    except ProcessLookupError: pass
                    signal.pidfd_send_signal(fd, signal.SIGKILL)
                    os.waitpid(pid, os.WNOHANG)
                finally:
                    os.close(fd)
            except (ProcessLookupError, ChildProcessError):
                pass
        time.sleep(0.05)
    return not child_pids()


class Runner:
    def __init__(self, root, clock=time.monotonic):
        self.root = Path(root); self.clock = clock; self.cancelled = False
        self.commands = []

    def interrupt(self, signum, frame):
        self.cancelled = True

    def run(self, label, argv, cwd, active, end, env=None, merged=False):
        if self.cancelled or self.clock() >= active:
            raise ValueError('cancelled/expired before command start')
        out = self.root / (label + '.stdout'); err = self.root / (label + '.stderr')
        start = self.clock(); timed_out = False; bounded = True
        row = {'argv': [str(a) for a in argv], 'cwd': str(cwd), 'start': start}
        self.commands.append(row)
        with out.open('xb') as stdout, err.open('xb') as stderr:
            proc = subprocess.Popen(argv, cwd=cwd, env=env, stdout=stdout, stderr=stdout if merged else stderr, start_new_session=True)
            fd = os.pidfd_open(proc.pid)
            try:
                while proc.poll() is None:
                    bounded = max(out.stat().st_size, err.stat().st_size) <= LIMIT
                    if self.cancelled or self.clock() >= active or not bounded:
                        timed_out = self.clock() >= active
                        try: os.killpg(proc.pid, signal.SIGTERM)
                        except ProcessLookupError: pass
                        break
                    time.sleep(0.1)
                try:
                    code = proc.wait(timeout=max(0.01, min(10, end - self.clock())))
                except subprocess.TimeoutExpired:
                    try: os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError: pass
                    signal.pidfd_send_signal(fd, signal.SIGKILL)
                    code = proc.wait(timeout=max(0.01, end - self.clock()))
            finally:
                os.close(fd)
        clean = cleanup(end)
        bounded = bounded and max(out.stat().st_size, err.stat().st_size) <= LIMIT
        row.update(exit=code, end=self.clock(), timed_out=timed_out,
                   cancelled=self.cancelled, output_complete=bounded, cleanup_confirmed=clean,
                   stdout_sha256=sha(out), stderr_sha256=sha(err))
        write_json(self.root / (label + '.json'), row)
        if not clean:
            raise ValueError('owned cleanup indeterminate; stop')
        return row

    def checked(self, *args, **kwargs):
        row = self.run(*args, **kwargs)
        if row['exit'] != 0 or row['timed_out'] or row['cancelled'] or not row['output_complete']:
            raise ValueError('command incomplete or failed: ' + args[0])
        return row
