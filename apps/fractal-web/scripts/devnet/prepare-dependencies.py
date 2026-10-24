#!/usr/bin/env python3
"""Reuse installed immutable dependencies; redirect workspace links into the pinned worktree."""
import errno
import hashlib
import json
import os
import shutil
import stat
import sys
from pathlib import Path
from dependency_cache import install_key, verify_generation


def destination_path(snapshot, relative):
    """Never traverse a snapshot symlink when creating copied trees."""
    destination = snapshot / relative
    for path in (destination, *destination.parents):
        if path.is_symlink():
            raise ValueError('snapshot copy destination has a symlink ancestor')
        if path == snapshot:
            break
    if not destination.resolve().is_relative_to(snapshot):
        raise ValueError('snapshot copy destination escapes snapshot')
    return destination


def prepare(source, snapshot, manifest_path, generation, expected):
    source = source.resolve(strict=True)
    if snapshot.is_symlink():
        raise ValueError('snapshot root must not be a symlink')
    snapshot = snapshot.resolve(strict=True)
    if install_key(source) != expected:
        raise ValueError('source install key differs from requested dependency cache key')
    inventory = verify_generation(generation, expected)
    member_store = source / '.devenv/candidate-members'
    if member_store.resolve() != member_store:
        raise ValueError('candidate member store must not resolve outside its selected path')
    members = json.loads((source / 'megarepo.lock').read_text())['members']
    member_paths = []
    member_copies = {}
    for name in members:
        relative = Path(name)
        if (relative.is_absolute() or '..' in relative.parts or not relative.parts
                or relative.as_posix() != name):
            raise ValueError(f'invalid member path: {name}')
        mount = source / 'repos' / relative
        target = mount.resolve()
        if (not mount.is_symlink() or not target.is_dir() or target == member_store
                or not target.is_relative_to(member_store)):
            raise ValueError(f'member must link beneath the candidate-local store: {name}')
        dst = destination_path(snapshot, target.relative_to(source))
        if dst.exists():
            raise ValueError('snapshot member source destination already exists')
        link = snapshot / mount.relative_to(source)
        destination_path(snapshot, link.parent.relative_to(snapshot))
        if link.exists() and not link.is_symlink():
            raise ValueError(f'snapshot member mount is not a symlink: {name}')
        member_copies[target] = dst
        member_paths.append((link, dst))
    # Validate every destination before copying any overlay or dependency files.
    dependencies = [
        (generation / relative, destination_path(snapshot, relative))
        for relative in inventory
        if not (source / relative).is_relative_to(member_store)
        or any((source / relative).is_relative_to(target) for target in member_copies)
    ]
    for _, destination in dependencies:
        if destination.exists():
            raise ValueError('snapshot dependency destination already exists')
    overlays = []
    for relative in ('apps/fractal-web/private', 'apps/fractal-web/vendor'):
        src, dst = source / relative, destination_path(snapshot, relative)
        if src.is_symlink() or not src.resolve().is_relative_to(source):
            raise ValueError('source overlay copy root escapes source or is a symlink')
        if src.is_dir() and not dst.exists():
            overlays.append((src, dst))
    manifest = {}
    for src, dst in overlays:
        shutil.copytree(src, dst, symlinks=True, ignore=shutil.ignore_patterns('node_modules'))
        for root, _, files in os.walk(dst):
            for name in files:
                file = Path(root) / name
                manifest[str(file.relative_to(snapshot))] = (
                    {'symlink': os.readlink(file)} if file.is_symlink()
                    else {'sha256': hashlib.sha256(file.read_bytes()).hexdigest()}
                )

    def redirect_workspace_links(dst):
        for copied_root, copied_dirs, files in os.walk(dst):
            # Only private snapshot directory inodes become writable; payload
            # hardlinks keep the generation's read-only modes.
            directory = Path(copied_root)
            directory.chmod(stat.S_IMODE(directory.stat().st_mode) | stat.S_IWUSR)
            for name in copied_dirs + files:
                link = Path(copied_root) / name
                if not link.is_symlink():
                    continue
                # Resolve relative links from their original source location,
                # never from the immutable generation or the snapshot.
                original = source / link.relative_to(snapshot)
                target = Path(os.path.abspath(original.parent / os.readlink(link)))
                if target.is_relative_to(source):
                    replacement = snapshot / target.relative_to(source)
                    link.unlink()
                    link.symlink_to(os.path.relpath(replacement, link.parent))

    for src, dst in member_copies.items():
        # Mutable member sources are copied, not linked; dependency trees are
        # installed separately and exclusively from the verified generation.
        destination_path(snapshot, dst.relative_to(snapshot))
        shutil.copytree(src, dst, symlinks=True, copy_function=shutil.copy2,
                        ignore=shutil.ignore_patterns('.git', 'node_modules'))
        redirect_workspace_links(dst)
    for link, dst in member_paths:
        destination_path(snapshot, link.parent.relative_to(snapshot))
        link.parent.mkdir(parents=True, exist_ok=True)
        if link.is_symlink():
            link.unlink()
        link.symlink_to(os.path.relpath(dst, link.parent))
    dependency_files = {'linked': 0, 'copied': 0}

    def copy_dependency(src, dst):
        try:
            result = os.link(src, dst)
            dependency_files['linked'] += 1
            return result
        except OSError as error:
            if error.errno != errno.EXDEV:
                raise
            if dependency_files['copied'] == 0:
                print('WARNING: EXDEV dependency copy fallback; copied files are counted in the receipt',
                      file=sys.stderr, flush=True)
            result = shutil.copy2(src, dst)
            dependency_files['copied'] += 1
            return result

    for src, dst in dependencies:
        # An overlay may have created destination parents; check again before copying.
        destination_path(snapshot, dst.relative_to(snapshot))
        shutil.copytree(src, dst, symlinks=True, copy_function=copy_dependency)
        redirect_workspace_links(dst)

    # Resolve only after every dependency tree and workspace redirection is in place:
    # a source-relative link can escape through a snapshot workspace ancestor.
    generation_root = generation.resolve(strict=True)
    for _, dst in dependencies:
        for copied_root, copied_dirs, files in os.walk(dst):
            for name in copied_dirs + files:
                link = Path(copied_root) / name
                if not link.is_symlink():
                    continue
                target = link.resolve(strict=False)
                if not (target.is_relative_to(snapshot) or target.is_relative_to(generation_root)):
                    raise ValueError(f'dependency symlink escapes snapshot and generation: {link}')

    print(f"prepare-dependencies: key={expected} linked={dependency_files['linked']} copied={dependency_files['copied']}" +
          (' WARNING: EXDEV copy fallback' if dependency_files['copied'] else ''),
          file=sys.stderr, flush=True)
    manifest_path.write_text(json.dumps({
        'copiedIgnoredOverlayFiles': manifest, 'dependencyKey': expected,
        'dependencyFiles': dependency_files,
    }, indent=2) + '\n')


if __name__ == '__main__':
    if len(sys.argv) != 6:
        raise SystemExit('Usage: prepare-dependencies.py SOURCE SNAPSHOT MANIFEST GENERATION EXPECTED_KEY')
    prepare(*(Path(value) for value in sys.argv[1:5]), sys.argv[5])
