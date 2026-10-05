import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

source = Path(sys.argv.pop(1))
expect_regression = '--expect-regression' in sys.argv
if expect_regression:
    sys.argv.remove('--expect-regression')
spec = importlib.util.spec_from_file_location('forwarder', source)
forwarder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(forwarder)
TARGET = 'agent/eval/operator/check/operator'


class Runner:
    def __init__(self, messages, operator=True):
        self.messages = messages
        self.operator = operator
        self.sends = []
        self.archives = []
        self.fail_archive = False
        self.fail_send = False

    def __call__(self, argv):
        if 'agents' in argv:
            return json.dumps({'value': {'items': [{'id': TARGET, 'state': 'running'}] if self.operator else []}})
        if 'ls' in argv:
            return json.dumps(self.messages)
        if 'send' in argv:
            self.sends.append(argv)
            if self.fail_send:
                raise RuntimeError('temporary send failure')
        elif 'archive' in argv:
            self.archives.append(argv)
            if self.fail_archive:
                raise RuntimeError('temporary archive failure')
        else:
            raise AssertionError(argv)
        return '{}'


def mail(status='sent', sender='person/eval', recipient=None):
    return {'subject': 'message/quartz', 'from': sender, 'to': recipient or forwarder.SELF,
            'status': status, 'title': 'Test handoff', 'content': 'QUARTZ SIGNAL'}


class ForwardingTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.state = Path(self.directory.name) / 'state.json'

    def forward(self, runner):
        return forwarder.forward_once('st3', self.state, runner)

    def test_recent_sent_mail_is_forwarded_without_a_harness_receipt(self):
        runner = Runner([mail()])
        self.assertEqual(self.forward(runner), ['message/quartz'])
        self.assertEqual(len(runner.sends), 1)
        self.assertEqual(len(runner.archives), 1)
        self.assertIn('forward/message/quartz', runner.sends[0])
        self.assertIn(TARGET, runner.sends[0])

    def test_staged_and_delivered_unread_mail_remain_supported(self):
        for status in ('staged', 'delivered'):
            with self.subTest(status=status):
                self.state.unlink(missing_ok=True)
                runner = Runner([mail(status)])
                self.assertEqual(self.forward(runner), ['message/quartz'])
                self.assertEqual(len(runner.sends), 1)

    def test_retry_does_not_forward_twice(self):
        runner = Runner([mail()])
        self.forward(runner)
        self.forward(runner)
        self.assertEqual(len(runner.sends), 1)

    def test_archive_failure_preserves_forward_progress(self):
        runner = Runner([mail()])
        runner.fail_archive = True
        with self.assertRaises(RuntimeError):
            self.forward(runner)
        runner.fail_archive = False
        self.forward(runner)
        self.assertEqual(len(runner.sends), 1)
        self.assertEqual(len(runner.archives), 2)

    def test_send_failure_retries_the_same_idempotency_key(self):
        runner = Runner([mail()])
        runner.fail_send = True
        with self.assertRaises(RuntimeError):
            self.forward(runner)
        self.assertFalse(self.state.exists())
        self.assertEqual(runner.archives, [])
        runner.fail_send = False
        self.forward(runner)
        self.assertEqual(runner.sends[0], runner.sends[1])

    def test_no_operator_preserves_mail(self):
        runner = Runner([mail()], operator=False)
        self.assertEqual(self.forward(runner), [])
        self.assertEqual(runner.sends, [])
        self.assertEqual(runner.archives, [])

    def test_read_closed_wrong_recipient_and_looping_senders_are_skipped(self):
        runner = Runner([mail('read'), mail('closed'), mail('unknown'),
                         mail(sender=forwarder.SELF), mail(sender=TARGET),
                         mail(recipient='agent/eval/other')])
        self.assertEqual(self.forward(runner), [])
        self.assertEqual(runner.sends, [])
        self.assertEqual(runner.archives, [])


if expect_regression:
    result = unittest.TextTestRunner(verbosity=2).run(unittest.TestSuite([
        ForwardingTests('test_recent_sent_mail_is_forwarded_without_a_harness_receipt')]))
    if result.wasSuccessful() or result.errors or len(result.failures) != 1:
        raise SystemExit('the original program must fail only the sent-mail regression')
    print('Expected original-program regression confirmed.')
else:
    unittest.main(verbosity=2)
