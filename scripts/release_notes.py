#!/usr/bin/env python3
"""Render source-pinned notes; incomplete impact metadata blocks publication only."""
import argparse
import json
import math
from pathlib import Path
import re
import subprocess
import sys

FIELDS = {
    'replay': 'Replay',
    'schema': 'Database schema',
    'rules': 'Checkpoint rules and fleet coordination',
    'compatibility': 'Client, protocol and harness compatibility',
    'downtime': 'Service interruption',
    'manual': 'Manual steps',
    'recovery': 'Rollback or roll-forward',
}
VERSIONS = {
    'schema': ('crates/smallclaims/src/store.rs', r'pub const SCHEMA_VERSION:.*?user_version\s*=\s*(\d+)'),
    'rules': ('crates/st3/src/store/checkpoint_rules.rs', r'pub const RULES_VERSION:\s*u32\s*=\s*(\d+)'),
}
FRAGMENTS = 'release-notes/'
SHA = re.compile(r'[0-9a-f]{40}')


class ImpactError(ValueError):
    pass


def git(repo, *args):
    return subprocess.check_output(['git', *args], cwd=repo, text=True).strip()


def resolve(repo, ref):
    return git(repo, 'rev-parse', '--verify', '--end-of-options', f'{ref}^{{commit}}')


def source_text(repo, source, path):
    return subprocess.check_output(['git', 'show', f'{source}:{path}'], cwd=repo, text=True)


def text(value, label):
    if not isinstance(value, str) or not value.strip() or len(value) > 8000:
        raise ImpactError(f'{label}: supply a nonempty string (at most 8000 characters)')


def validate(fragment, path):
    required = {'version', 'summary', *FIELDS}
    if not isinstance(fragment, dict) or required - fragment.keys():
        raise ImpactError(f'{path}: missing required fields {sorted(required - fragment.keys()) if isinstance(fragment, dict) else sorted(required)}')
    if fragment.keys() - required - {'commits'} or type(fragment['version']) is not int or fragment['version'] != 1:
        raise ImpactError(f'{path}: unsupported fields or fragment version')
    text(fragment['summary'], f'{path}.summary')
    commits = fragment.get('commits', [])
    if not isinstance(commits, list) or any(not isinstance(c, str) or not SHA.fullmatch(c) for c in commits):
        raise ImpactError(f'{path}.commits: use full source commit hashes')
    for field in FIELDS:
        impact = fragment[field]
        allowed = {'status', 'detail'}
        if field in VERSIONS:
            allowed.add('transitions')
        if field == 'downtime':
            allowed.add('observations')
        if not isinstance(impact, dict) or impact.keys() - allowed or {'status', 'detail'} - impact.keys():
            raise ImpactError(f'{path}.{field}: expected status and detail')
        if impact['status'] not in ('none', 'changed', 'unknown'):
            raise ImpactError(f'{path}.{field}: status must be none, changed or unknown')
        text(impact['detail'], f'{path}.{field}.detail')
        transitions = impact.get('transitions', [])
        if not isinstance(transitions, list) or any(
            not isinstance(pair, list) or len(pair) != 2 or any(type(v) is not int or v < 0 for v in pair)
            for pair in transitions
        ):
            raise ImpactError(f'{path}.{field}: transitions are [from, to] integer pairs')
        if transitions and impact['status'] == 'none':
            raise ImpactError(f'{path}.{field}: none cannot declare version transitions')
        observations = impact.get('observations', [])
        if not isinstance(observations, list):
            raise ImpactError(f'{path}.downtime.observations: expected a list')
        for observation in observations:
            expected = {'source', 'platform', 'graph_size', 'method', 'api_seconds', 'health_seconds'}
            if not isinstance(observation, dict) or observation.keys() != expected:
                raise ImpactError(f'{path}.downtime: observations require source/platform/graph_size/method/api_seconds/health_seconds')
            if not isinstance(observation['source'], str) or not SHA.fullmatch(observation['source']):
                raise ImpactError(f'{path}.downtime: observation source must be an exact commit')
            for label in ('platform', 'graph_size', 'method'):
                text(observation[label], f'{path}.downtime.{label}')
            times = [observation['api_seconds'], observation['health_seconds']]
            if all(v is None for v in times) or any(
                v is not None and (type(v) not in (int, float) or not math.isfinite(v) or v < 0) for v in times
            ):
                raise ImpactError(f'{path}.downtime: give finite nonnegative measured seconds; null means unmeasured')
        if observations and impact['status'] == 'none':
            raise ImpactError(f'{path}.downtime: measurements require changed or unknown status')


def version(repo, source, field):
    path, pattern = VERSIONS[field]
    content = source_text(repo, source, path)
    match = re.search(pattern, content)
    if not match:
        raise ImpactError(f'{source[:12]}: cannot identify {field} version in {path}')
    return int(match[1])


def collect(repo, since, source):
    since, source = resolve(repo, since), resolve(repo, source)
    if subprocess.run(['git', 'merge-base', '--is-ancestor', since, source], cwd=repo).returncode:
        raise ImpactError('previous release source must be an ancestor of the candidate')
    commits = git(repo, 'rev-list', '--first-parent', '--reverse', f'{since}..{source}').splitlines()
    # Read the final candidate tree: uncommitted edits and a newer checkout never alter notes.
    paths = [path for path in git(repo, 'ls-tree', '-r', '--name-only', source, '--', FRAGMENTS).splitlines()
             if path.startswith(FRAGMENTS) and path.endswith('.json')]
    fragments = {}
    for path in paths:
        try:
            fragment = json.loads(source_text(repo, source, path))
        except json.JSONDecodeError as error:
            raise ImpactError(f'{path}: invalid JSON: {error}') from error
        validate(fragment, path)
        fragments[path] = fragment
    coverage = {commit: set() for commit in commits}
    for commit in commits:
        changed = git(repo, 'diff', '--no-renames', '--name-only', '--diff-filter=AM', f'{commit}^1', commit, '--', FRAGMENTS).splitlines()
        coverage[commit].update(path for path in changed if path in fragments)
    for path, fragment in fragments.items():
        for commit in fragment.get('commits', []):
            if commit in coverage:
                coverage[commit].add(path)
    missing = [commit for commit, entries in coverage.items() if not entries]
    if missing:
        raise ImpactError('Missing upgrade classifications (add a fragment, or backfill its commits list):\n' + '\n'.join(
            f'  {commit} {git(repo, "show", "-s", "--format=%s", commit)}' for commit in missing
        ))
    changes = []
    for commit in commits:
        for field in VERSIONS:
            before, after = version(repo, f'{commit}^1', field), version(repo, commit, field)
            if before == after:
                continue
            declared = any(
                fragments[path][field]['status'] != 'none' and [before, after] in fragments[path][field].get('transitions', [])
                for path in coverage[commit]
            )
            if not declared:
                raise ImpactError(f'{commit}: {field} version {before} -> {after} lacks matching impact metadata')
            changes.append({'commit': commit, 'field': field, 'from': before, 'to': after})
    selected = sorted({path for entries in coverage.values() for path in entries})
    return {
        'version': 1, 'previous_source': since, 'source': source,
        'coverage': {commit: sorted(paths) for commit, paths in coverage.items()},
        'version_changes': changes,
        'fragments': [{'path': path, **fragments[path]} for path in selected],
        'versions': {field: [version(repo, since, field), version(repo, source, field)] for field in VERSIONS},
    }


def render(repo, previous, source, tag, repository='compoundingtech/smalltalk', since=None):
    report = collect(repo, since or previous, source)
    source, since = report['source'], report['previous_source']
    lines = [f'Smalltalk {tag} at `{source}`. Artifacts identify this exact source in RELEASE.json.', '',
             '## Upgrade impact', '',
             f'From **{previous}** (`{since}`) to **{tag}** (`{source}`).',
             'This classification covers the entire source interval. Earlier starting releases may have additional requirements.', '',
             f'Database schema: **{report["versions"]["schema"][0]} → {report["versions"]["schema"][1]}**. '
             f'Checkpoint rules: **{report["versions"]["rules"][0]} → {report["versions"]["rules"][1]}**. '
             'An unchanged number does not establish rollback compatibility or exclude table changes.', '']
    for field, label in FIELDS.items():
        entries = [fragment for fragment in report['fragments'] if fragment[field]['status'] != 'none']
        lines.append(f'### {label}')
        lines.append('')
        if not entries:
            lines.append('None declared by the classified changes in this interval. Follow the standard upgrade procedure.')
        for fragment in entries:
            impact = fragment[field]
            transitions = ''.join(f' ({before} → {after})' for before, after in impact.get('transitions', []))
            lines.append(f'- **{impact["status"].capitalize()}{transitions}:** {impact["detail"]} '
                         f'[{fragment["summary"]}](https://github.com/{repository}/blob/{source}/{fragment["path"]})')
            for observation in impact.get('observations', []):
                api = 'unmeasured' if observation['api_seconds'] is None else f'{observation["api_seconds"]} seconds'
                health = 'unmeasured' if observation['health_seconds'] is None else f'{observation["health_seconds"]} seconds'
                lines.append(f'  - Observed source `{observation["source"]}`, {observation["platform"]}, '
                             f'{observation["graph_size"]}: restart-to-API {api}; restart-to-health {health}. '
                             f'Method: {observation["method"]}. These are contextual observations, not a downtime guarantee.')
        lines.append('')
    lines.extend([f'## Changes since {previous}', ''])
    for commit in report['coverage']:
        raw = git(repo, 'show', '-s', '--format=%s%n%b', commit)
        subject, _, body = raw.partition('\n')
        merge = re.match(r'Merge pull request #(\d+) from ', subject)
        title = body.strip().splitlines()[0] if merge and body.strip() else subject
        link = f'https://github.com/{repository}/pull/{merge.group(1)}' if merge else f'https://github.com/{repository}/commit/{commit}'
        lines.append(f'- [{title}]({link})')
    if not report['coverage']:
        lines.append('- No source changes.')
    guide = source_text(repo, source, 'docs/st3/binary-releases.md')
    lines.extend(['', '---', '', guide.rstrip(), '', f'Exact source: `{source}`. See RELEASE.json for the PTY revision and targets.', ''])
    return '\n'.join(lines), report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', required=True)
    parser.add_argument('--tag', required=True)
    parser.add_argument('--previous', help='Previous public release; defaults to GitHub latest')
    parser.add_argument('--output', type=Path, help='Notes file; default stdout')
    parser.add_argument('--manifest', type=Path, help='Optional machine-readable impact report')
    parser.add_argument('--repo', type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument('--repository', default='compoundingtech/smalltalk')
    args = parser.parse_args()
    previous = args.previous or subprocess.check_output(
        ['gh', 'release', 'view', '--repo', args.repository, '--json', 'tagName', '--jq', '.tagName'], text=True
    ).strip()
    try:
        notes, report = render(args.repo, previous, args.source, args.tag, args.repository)
    except ImpactError as error:
        print(f'release notes: {error}', file=sys.stderr)
        return 1
    if args.output:
        args.output.write_text(notes)
    else:
        print(notes, end='')
    if args.manifest:
        args.manifest.write_text(json.dumps(report, indent=2) + '\n')
    return 0


if __name__ == '__main__':
    sys.exit(main())
