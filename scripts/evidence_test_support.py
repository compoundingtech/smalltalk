"""Shared by the evidence script tests. Not a script: tests import it from the scripts directory."""
import os
from pathlib import Path
import sys
import tempfile

sys.dont_write_bytecode = True


# What the signer refuses to start with (scripts/evidence-sign REFUSED_ENVIRONMENT). Some hosts set LD_PRELOAD for
# every process, so a test that expects the signer to run first removes these from its own environment.
LOADER_VARIABLES = ('LD_PRELOAD', 'LD_AUDIT', 'DYLD_INSERT_LIBRARIES', 'GCONV_PATH', 'NLSPATH',
                    'PYTHONPATH', 'PYTHONSTARTUP', 'PYTHONHOME')


def clean_loader_environment():
    for name in LOADER_VARIABLES:
        os.environ.pop(name, None)


def tool_path():
    """Directories of the real git and ssh-keygen on this machine, usable as the signer's own path.

    The signer accepts only directories it can trust: not under HOME or a temporary directory, not
    writable by the user running it unless in /nix/store. A Nix sandbox keeps the tools in /nix/store,
    outside the signer's fixed system directories, so the tests hand it these.
    """
    unsafe = [Path.home(), Path(tempfile.gettempdir()), Path('/tmp'), Path('/var/tmp')]
    found = []
    for tool in ('git', 'ssh-keygen'):
        for directory in os.environ['PATH'].split(':'):
            real = os.path.realpath(directory)
            if 'recorder' in directory or not (Path(directory) / tool).exists():
                continue
            if any(real == str(root) or real.startswith(str(root).rstrip('/') + '/') for root in unsafe):
                continue
            if not real.startswith('/nix/store/') and os.access(real, os.W_OK):
                continue
            found.append(directory)
            break
    return ':'.join(dict.fromkeys(found))
