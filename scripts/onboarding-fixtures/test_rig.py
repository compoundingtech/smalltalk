#!/usr/bin/env python3
"""Checks for false-positive consent evidence and pre-launch archive/VM guards."""
import hashlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import pty
import shutil
import subprocess
import tarfile
import tempfile
import time
from types import SimpleNamespace
import unittest

SCRIPTS=Path(__file__).resolve().parents[1]
loader=importlib.machinery.SourceFileLoader("onboarding_rig",str(SCRIPTS/"onboarding-e2e"))
spec=importlib.util.spec_from_loader(loader.name,loader)
rig=importlib.util.module_from_spec(spec)
loader.exec_module(rig)

class RigChecks(unittest.TestCase):
    def test_bad_hash_rejected_before_machine_creation(self):
        with tempfile.TemporaryDirectory() as d:
            archive=Path(d,"bad.tar.gz"); archive.write_bytes(b"wrong bytes")
            with self.assertRaisesRegex(RuntimeError,"SHA256 mismatch"):
                rig.load_archive(SimpleNamespace(archive=str(archive),sha256="0"*64),d)

    def test_archive_traversal_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            archive=Path(d,"escape.tar.gz")
            with tarfile.open(archive,"w:gz") as tar:
                member=tarfile.TarInfo("../escape"); member.size=4
                tar.addfile(member,io.BytesIO(b"oops"))
            digest=hashlib.sha256(archive.read_bytes()).hexdigest()
            with self.assertRaisesRegex(RuntimeError,"unsafe archive member"):
                rig.load_archive(SimpleNamespace(archive=str(archive),sha256=digest),d)

    def test_vm_refused_in_ci_before_output_creation(self):
        with tempfile.TemporaryDirectory() as d:
            out=Path(d,"report")
            r=subprocess.run([str(SCRIPTS/"onboarding-vm-test"),"baseline","--backend","incus","--out",str(out)],
                env={**os.environ,"CI":"true"},capture_output=True,text=True)
            self.assertEqual(r.returncode,2)
            self.assertIn("refused in CI",r.stderr)
            self.assertFalse(out.exists())

    def test_mission_identity_ignores_runtime_changes(self):
        before={"items":[{"id":"mission/st/onboarding","runs":["mission-run/one"],"state":"working"}]}
        after={"items":[{"id":"mission/st/onboarding","runs":["mission-run/one"],"state":"waiting"}]}
        self.assertEqual(rig.mission_runs(before),rig.mission_runs(after))
        after["items"][0]["runs"].append("mission-run/two")
        self.assertNotEqual(rig.mission_runs(before),rig.mission_runs(after))

    def test_read_receipt_must_match_wake_and_new_incarnation(self):
        old={"event":"read","reference":"message/abc","exit":0,"incarnation":"old"}
        self.assertFalse(rig.successful_read([old],"message/abc","new"))
        self.assertFalse(rig.successful_read([{**old,"incarnation":"new"}],"message/def","new"))
        self.assertFalse(rig.successful_read([{**old,"incarnation":"new","exit":2}],"message/abc","new"))
        self.assertTrue(rig.successful_read([old,{**old,"incarnation":"new"}],"message/abc","new"))

    def test_focus_probe_rejects_sidebar_and_unfocused_composer(self):
        code='import os,tty; tty.setraw(0); os.write(1,b"\\x1b[?1049h working Expert sidebar Message Expert "+"·".encode()+b" click");\nwhile True:\n value=os.read(0,100)\n if b"\\x11" in value: os.write(1,b"\\x1b[?1049l"); break\n if b"home" in value: os.write(1,b"Now Nothing needs you")\n'
        request={"argv":["python3","-c",code],"timeout":5,"expect_expert":True,"expert_focus_timeout":0.1}
        r=subprocess.run(["python3","-c",rig.PTY_PROBE,json.dumps(request)],capture_output=True,text=True,timeout=8)
        self.assertEqual(r.returncode,0)
        value=json.loads(r.stdout)
        self.assertFalse(value["expert_focused"])
        self.assertTrue(value["home_navigation"])
        self.assertTrue(value["restored"])

    def test_focus_probe_accepts_focused_expert_before_navigation(self):
        code='import os,tty; tty.setraw(0); os.write(1,b"\\x1b[?1049h working Message Expert "+"·".encode()+b" type or click");\nwhile True:\n value=os.read(0,100)\n if b"\\x11" in value: os.write(1,b"\\x1b[?1049l"); break\n if b"home" in value: os.write(1,b"Now Nothing needs you")\n'
        request={"argv":["python3","-c",code],"timeout":5,"expect_expert":True,"expert_focus_timeout":0.1}
        r=subprocess.run(["python3","-c",rig.PTY_PROBE,json.dumps(request)],capture_output=True,text=True,timeout=8)
        self.assertEqual(r.returncode,0)
        value=json.loads(r.stdout)
        self.assertTrue(value["expert_focused"])
        self.assertNotIn("Nothing needs you",value["initial_screen"])
        self.assertTrue(value["home_frame"])

    def test_fixture_cannot_start_before_consent(self):
        with tempfile.TemporaryDirectory() as d:
            directory=Path(d)
            for name in ("stubmodel.py","stub-claude.py"):
                shutil.copyfile(SCRIPTS/"st3-boot-canaries"/name,directory/name)
            shutil.copyfile(SCRIPTS/"onboarding-fixtures/claude.py",directory/"claude.py")
            master,slave=pty.openpty()
            process=subprocess.Popen(["python3",str(directory/"claude.py"),"--dangerously-load-development-channels=server:st3"],
                cwd=d,env={**os.environ,"HOME":d,"CLAUDE_CONFIG_DIR":str(directory/"managed"),"ST_AGENT":"agent/fixture","ST3_INCARNATION":"fixture-current"},stdin=slave,stdout=slave,stderr=slave)
            os.close(slave)
            receipt=directory/"receipts-agent-fixture.jsonl"
            def records():
                return [json.loads(line) for line in receipt.read_text().splitlines()] if receipt.exists() else []
            try:
                deadline=time.monotonic()+5
                while time.monotonic()<deadline and not records(): time.sleep(0.05)
                self.assertTrue(records())
                self.assertFalse(any(r["event"]=="started" for r in records()))
                os.write(master,b"\r")
                deadline=time.monotonic()+5
                while time.monotonic()<deadline and not any(r["event"]=="started" for r in records()): time.sleep(0.05)
                self.assertTrue(any(r["event"]=="development-consent" and r["accepted"] for r in records()))
                self.assertTrue(any(r["event"]=="started" for r in records()))
                self.assertTrue(all(r.get("incarnation")=="fixture-current" for r in records()))
            finally:
                process.terminate(); process.wait(timeout=5); os.close(master)

    def test_packaged_selectors_resolve_the_native_mcp_after_consent(self):
        variants=(['--channels','plugin:st-channel@st'],
                  ['--dangerously-load-development-channels','plugin:st-channel@st'],
                  ['--dangerously-load-development-channels=plugin:st-channel@st'])
        for args in variants:
            with self.subTest(args=args),tempfile.TemporaryDirectory() as d:
                directory=Path(d)
                shutil.copyfile(SCRIPTS/'st3-boot-canaries/stubmodel.py',directory/'stubmodel.py')
                shutil.copyfile(SCRIPTS/'onboarding-fixtures/claude.py',directory/'claude.py')
                (directory/'.claude').mkdir()
                (directory/'.claude/onboarding-fixture.json').write_text(json.dumps({'plugins':['st-channel@st'],'marketplaces':[]}))
                (directory/'stub-claude.py').write_text('import json,sys\nfrom pathlib import Path\nPath("argv.json").write_text(json.dumps(sys.argv[1:]))\n')
                master,slave=pty.openpty()
                process=subprocess.Popen(['python3',str(directory/'claude.py'),*args],cwd=d,
                    env={**os.environ,'HOME':d,'ST_AGENT':'agent/fixture','ST3_BIN':'/bin/true'},stdin=slave,stdout=slave,stderr=slave)
                os.close(slave)
                try:
                    if args[0].startswith('--dangerously'):
                        deadline=time.monotonic()+5
                        receipt=directory/'receipts-agent-fixture.jsonl'
                        while time.monotonic()<deadline and not receipt.exists(): time.sleep(0.05)
                        self.assertTrue(receipt.exists())
                        self.assertFalse((directory/'argv.json').exists())
                        os.write(master,b'\r')
                    self.assertEqual(process.wait(timeout=5),0)
                    actual=json.loads((directory/'argv.json').read_text())
                    config=json.loads(actual[actual.index('--mcp-config')+1])
                    self.assertEqual(config['mcpServers']['st3'],{'command':'/bin/true','args':['driver','claude-mcp']})
                finally:
                    if process.poll() is None: process.terminate(); process.wait(timeout=5)
                    os.close(master)

    def test_managed_claude_transcript_can_resume_after_restart(self):
        with tempfile.TemporaryDirectory() as d:
            directory=Path(d); config=directory/"managed"; home=directory/"home"; home.mkdir()
            for name in ("stubmodel.py","stub-claude.py"):
                shutil.copyfile(SCRIPTS/"st3-boot-canaries"/name,directory/name)
            shutil.copyfile(SCRIPTS/"onboarding-fixtures/claude.py",directory/"claude.py")
            env={**os.environ,"HOME":str(home),"CLAUDE_CONFIG_DIR":str(config),"ST_AGENT":"agent/fixture","ST3_INCARNATION":"before"}
            receipt=directory/"receipts-agent-fixture.jsonl"
            def records(): return [json.loads(line) for line in receipt.read_text().splitlines()] if receipt.exists() else []
            process=subprocess.Popen(["python3",str(directory/"claude.py")],cwd=d,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            try:
                deadline=time.monotonic()+5
                while time.monotonic()<deadline and not list(config.glob("projects/*/*.jsonl")): time.sleep(0.05)
                transcripts=list(config.glob("projects/*/*.jsonl"))
                self.assertEqual(len(transcripts),1)
                session=transcripts[0].stem
                self.assertFalse((home/".claude/projects").exists())
            finally: process.terminate(); process.communicate(timeout=5)
            process=subprocess.Popen(["python3",str(directory/"claude.py"),"--resume",session],cwd=d,env={**env,"ST3_INCARNATION":"after"},stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            try:
                deadline=time.monotonic()+5
                while time.monotonic()<deadline and not any(r["event"]=="session" and r.get("resumed") for r in records()): time.sleep(0.05)
                self.assertTrue(any(r["event"]=="session" and r.get("resumed") and r.get("incarnation")=="after" for r in records()))
                self.assertFalse(any(r["event"]=="session-missing" for r in records()))
            finally: process.terminate(); process.communicate(timeout=5)

if __name__=="__main__": unittest.main()
