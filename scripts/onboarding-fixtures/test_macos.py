#!/usr/bin/env python3
"""Linux-runnable negative controls for the manual Mac runner; never starts a VM."""
import importlib.machinery
import importlib.util
import contextlib
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock,patch

SCRIPTS=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(SCRIPTS/'onboarding-fixtures'))
from macos import TartMachine,daemon_running
loader=importlib.machinery.SourceFileLoader('mac_rig',str(SCRIPTS/'onboarding-e2e'))
spec=importlib.util.spec_from_loader(loader.name,loader)
rig=importlib.util.module_from_spec(spec); loader.exec_module(rig)

class MacGuards(unittest.TestCase):
    def test_manual_alias_refuses_ci_without_creating_output(self):
        with tempfile.TemporaryDirectory() as d:
            out=Path(d)/'result'
            r=subprocess.run([str(SCRIPTS/'onboarding-mac-test'),'--archive','absent.tar.gz','--out',str(out)],env={**os.environ,'CI':'true'},capture_output=True,text=True)
            self.assertEqual(r.returncode,2)
            self.assertIn('refused in CI',r.stderr)
            self.assertFalse(out.exists())

    def test_wrong_archive_target_refused_before_clone(self):
        with tempfile.TemporaryDirectory() as d:
            key=Path(d)/'key'; key.write_text('fixture')
            out=Path(d)/'result'
            argv=['onboarding-mac-test','--backend','tart','--archive','fixture.tar.gz','--out',str(out),'--ssh-key',str(key),'--tart-image','clean']
            with patch.object(sys,'argv',argv),patch.object(rig.platform,'system',return_value='Darwin'),patch.object(rig.platform,'machine',return_value='arm64'),patch.object(rig.shutil,'which',return_value='/fixture'),patch.dict(os.environ,{'CI':'','GITHUB_ACTIONS':'','BUILDKITE':''}),patch.object(rig,'load_archive',return_value=(Path(d)/'fixture.tar.gz',{'build':{'target':'x86_64-unknown-linux-gnu'}})),patch.object(subprocess,'Popen') as start:
                with contextlib.redirect_stderr(io.StringIO()),self.assertRaises(SystemExit) as error: rig.main()
                self.assertEqual(error.exception.code,2)
                start.assert_not_called()
                self.assertFalse(out.exists())

    def test_failed_clone_never_deletes_existing_base(self):
        with tempfile.TemporaryDirectory() as d:
            machine=TartMachine(SimpleNamespace(tart_image='clean',keep=False),Path(d))
            machine.host=Mock(return_value={'exit':1,'stderr':'fixture clone failure'})
            with self.assertRaisesRegex(RuntimeError,'clone failed'): machine.start()
            machine.close()
            self.assertEqual(machine.host.call_count,1)
            self.assertEqual(machine.host.call_args.args,('clone','clean',machine.name))

    def test_failure_after_clone_removes_only_owned_unique_vm(self):
        with tempfile.TemporaryDirectory() as d:
            machine=TartMachine(SimpleNamespace(tart_image='clean',keep=False),Path(d))
            machine.host=Mock(side_effect=[{'exit':0,'stderr':''},{'exit':1,'stderr':'fixture configuration failure'},{'exit':0,'stderr':''}])
            with self.assertRaisesRegex(RuntimeError,'configuration failed'): machine.start()
            machine.close()
            self.assertEqual(machine.host.call_args.args,('delete',machine.name))
            self.assertNotEqual(machine.name,'clean')
            self.assertFalse(machine.created)

    def test_cleanup_stops_before_delete_and_keep_preserves_owned_vm(self):
        with tempfile.TemporaryDirectory() as d:
            args=SimpleNamespace(keep=False)
            machine=TartMachine(args,Path(d)); machine.created=True
            machine.process=Mock(); machine.process.poll.return_value=None
            machine.host=Mock(return_value={'exit':0,'stderr':''})
            machine.close()
            self.assertEqual([call.args for call in machine.host.call_args_list],[('stop',machine.name,'--timeout','30'),('delete',machine.name)])
            machine.created=True; args.keep=True; machine.host.reset_mock()
            machine.close(); machine.host.assert_not_called()
            self.assertTrue(machine.created)

    def test_ssh_is_noninteractive_and_transfer_path_is_shell_quoted(self):
        with tempfile.TemporaryDirectory() as d:
            machine=TartMachine(SimpleNamespace(ssh_key=Path(d)/'key with spaces'),Path(d)); machine.address='192.0.2.10'
            with patch.object(subprocess,'run',return_value=SimpleNamespace(returncode=0,stderr=b'')) as invoke:
                machine.send(b'fixture','/Users/ada/candidate $(touch unwanted)')
                argv=invoke.call_args.args[0]
                self.assertIn('BatchMode=yes',argv)
                self.assertIn('PasswordAuthentication=no',argv)
                self.assertIn('KbdInteractiveAuthentication=no',argv)
                self.assertIn('IdentitiesOnly=yes',argv)
                self.assertEqual(invoke.call_args.kwargs['input'],b'fixture')
                import shlex
                remote=shlex.split(argv[-1])
                self.assertEqual(remote[:2],['/bin/bash','-c'])
                self.assertEqual(remote[2],"mkdir -p ~/smalltalk-install; cat > '/Users/ada/candidate $(touch unwanted)'")

    def test_launchd_state_requires_actual_daemon_and_running_flags(self):
        status={'manager':'launchd-user','services':[{'name':'com.compoundingtech.st3','installed':True,'running':True,'state':'running'}]}
        self.assertTrue(daemon_running(status))
        for field,value in [('name','com.compoundingtech.st3.replication'),('installed',False),('running',False),('state','loaded')]:
            broken={'manager':status['manager'],'services':[{**status['services'][0],field:value}]}
            self.assertFalse(daemon_running(broken))
        self.assertFalse(daemon_running({**status,'manager':'systemd-user'}))

    def test_gui_not_ready_does_not_count_as_boot_success(self):
        with tempfile.TemporaryDirectory() as d:
            machine=TartMachine(SimpleNamespace(ssh_key=Path(d)/'key'),Path(d))
            machine.host=Mock(return_value={'exit':0,'stdout':'192.0.2.10','stderr':''})
            machine.run=Mock(side_effect=[{'exit':1},{'exit':0}])
            process=Mock(); process.poll.return_value=None
            with patch.object(subprocess,'Popen',return_value=process),patch('macos.time.sleep'):
                machine.boot()
            self.assertEqual(machine.run.call_count,2)
            self.assertIn('launchctl print',machine.run.call_args.args[0])
            machine.log.close()

if __name__=='__main__': unittest.main()
