#!/usr/bin/env python3
"""Forward everything sent to agent/eval/mail-router to the running operator seat.

Other seats send operations handoffs to the sender identity the deploy execs use, and that identity has no
harness, so on 2026-10-02 four handoffs sat unread. This program runs as that seat (a declared argv program,
like the delivery probes): it reads the seat's own mailbox every 20 seconds, forwards each unread message to the
current operator seat with an idempotency key (so a retry never duplicates), then archives the original. If no
operator seat is running it leaves the messages where they are and tries again later. Messages from the operator
seat itself and from this seat are never forwarded, so it cannot loop. Reversible: put `sleep infinity` back."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

SELF = 'agent/eval/mail-router'
OPERATOR = re.compile(r'agent/eval/operator/[^/]+/operator')
INTERVAL = 20


def run(argv, timeout=60):
    return subprocess.run(argv, check=True, timeout=timeout, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout


def current_operator(st3, runner=run):
    items = json.loads(runner([st3, '--daemon-wait', '5', '--json', 'agents', 'ls']))['value']['items']
    live = sorted(a['id'] for a in items if OPERATOR.fullmatch(a['id']) and a.get('state') == 'running')
    return live[-1] if live else None


def forward_once(st3, state_path, runner=run):
    """Forward the unread messages; return the ids forwarded in this pass."""
    state_path = Path(state_path)
    state = json.loads(state_path.read_text()) if state_path.exists() else {'forwarded': []}
    operator = current_operator(st3, runner)
    if operator is None:
        return []
    mailbox = json.loads(runner([st3, '--daemon-wait', '5', '--json', 'conversations', 'ls', '--as', SELF]))
    done = []
    for message in mailbox:
        message_id = message['subject']
        if message.get('status') not in ('sent', 'staged', 'delivered') or message.get('to') != SELF:
            continue
        if message.get('from') in (SELF, operator):
            continue
        if message_id not in state['forwarded']:
            body = 'Forwarded from %s. Original: %s from %s (reply to that sender, not to this address).\n\n%s' % (
                SELF, message_id, message.get('from'), message.get('content', ''))
            runner([st3, '--daemon-wait', '30', 'conversations', 'send', operator, '--from', SELF,
                    '--idempotency-key', 'forward/' + message_id,
                    '--subject', 'Forwarded: ' + (message.get('title') or message_id), '--body', body])
            state['forwarded'].append(message_id)
            state['forwarded'] = state['forwarded'][-2000:]
            state_path.parent.mkdir(parents=True, exist_ok=True)
            state_path.write_text(json.dumps(state))
        runner([st3, '--daemon-wait', '30', 'conversations', 'archive', message_id, '--as', SELF])
        done.append(message_id)
    return done


def main():
    st3 = os.environ.get('ST3_BIN') or str(Path.home() / '.local/bin/st3')
    state_path = Path.home() / '.local/state/st3/operations/mailbox-forward/state.json'
    while True:
        try:
            forwarded = forward_once(st3, state_path)
            if forwarded:
                print(time.strftime('%H:%M:%S'), 'forwarded', ' '.join(forwarded), flush=True)
        except Exception as exc:  # the seat must outlive a busy daemon
            print(time.strftime('%H:%M:%S'), 'forward pass failed:', exc, file=sys.stderr, flush=True)
        time.sleep(INTERVAL)


if __name__ == '__main__':
    main()
