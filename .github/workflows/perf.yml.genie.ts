import { defaultActionlintConfig, githubWorkflow, nixDevelopStep, plainFlakeSetupSteps } from '../../repos/effect-utils/genie/external.ts'
import { buildEnv, linuxStageRunner, readOnlyBinaryCaches } from './workspace-ci.ts'

const paired = "import json\nimport os\nfrom pathlib import Path\nimport shutil\nimport subprocess\nimport sys\nimport time\n\nmain_sha, head_sha = os.environ['PAIR_BASE'], os.environ['PAIR_HEAD']\nimport re\nassert all(re.fullmatch(r'[0-9a-f]{40}', sha) for sha in [main_sha, head_sha])\nroot = Path(os.environ['RUNNER_TEMP'])\nout = root / 'perf'\nout.mkdir(exist_ok=True)\nreports = {}\nbinaries = {}\nfor name, sha in [('head', head_sha)]:\n    subprocess.run(['git', 'checkout', '--detach', sha], check=True)\n    assert subprocess.check_output(['git', 'status', '--porcelain']).strip() == b''\n    # The load test does not initialize the existing profiler; this diagnostic-only\n    # test-loader patch enables it. Production library code remains the exact source SHA.\n    test_source = Path('crates/st3/tests/daemon_load.rs')\n    original = test_source.read_text()\n    marker = '    let scale = env_number(\"ST_LOAD_SCALE\", 1.0_f64);'\n    assert original.count(marker) == 1\n    test_source.write_text(original.replace(marker, '    st3::profile::init_from_env();\\n' + marker))\n    (out / 'profile-loader.patch').write_text(subprocess.check_output(['git', 'diff', '--', str(test_source)], text=True))\n    env = dict(os.environ, AGENT_SPEC_REVISION=sha)\n    env.pop('ST_AGENT', None)\n    with (out / f'paired-{name}-build.jsonl').open('w') as log:\n        subprocess.run(['cargo', 'test', '--release', '-p', 'st3', '--features', 'perf-load',\n                        '--test', 'perf_load', '--locked', '--no-run', '--message-format=json'],\n                       env=env, stdout=log, check=True)\n    artifacts = [json.loads(line) for line in (out / f'paired-{name}-build.jsonl').read_text().splitlines()]\n    executable = next(a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact'\n                      and a.get('executable') and a['target']['name'] == 'perf_load')\n    binaries[name] = root / f'paired-{name}-perf-load'\n    shutil.copy2(executable, binaries[name])\n    subprocess.run(['git', 'restore', '--source', sha, '--', str(test_source)], check=True)\n\n# Both builds are warm before measurement. These are the unchanged, independently built\n# main/head load tests on the same runner, checkout path, and generated-store recipe.\nfor name, sha in [('head', head_sha)]:\n    subprocess.run(['git', 'checkout', '--detach', sha], check=True)\n    report = out / f'paired-{name}.json'\n    env = dict(os.environ, ST_LOAD_GATE='1', ST_LOAD_SECONDS='300', ST_LOAD_REPORT=str(report),\n               ST_LOAD_BASELINE=str(root / 'perf-baseline'),\n               ST_BENCH_DIR=str(root / 'st-bench'), TMPDIR=str(root),\n               ST3_PROFILE_DIR=str(out / f'profile-{name}'), ST3_PROFILE_SLOW_MS='5')\n    env.pop('ST_AGENT', None)\n    start = time.monotonic()\n    with (out / f'paired-{name}.log').open('w') as log:\n        result = subprocess.run([str(binaries[name]), 'daemon_load::', '--nocapture'],\n                                env=env, stdout=log, stderr=subprocess.STDOUT)\n    reports[name] = json.loads(report.read_text())\n    reports[name]['source_sha'] = sha\n    reports[name]['test_exit'] = result.returncode\n    reports[name]['wall_seconds'] = time.monotonic() - start\n    print(name, sha, 'test exit', result.returncode, 'cores', reports[name]['daemon_cores'], flush=True)\n\nprofile_dir = out / 'profile-head'\nassert (profile_dir / 'slow.jsonl').is_file(), 'profiler did not initialize'\nassert (profile_dir / 'totals.json').is_file(), 'profiler did not produce totals'\nresult = {'head': reports['head'], 'runner_name': os.environ.get('RUNNER_NAME'),\n          'note': 'Latency profiling only. Production library is exact head SHA; a recorded diagnostic-only load-test initialization patch activates the existing profiler. Instrumented results are not unprofiled main/head validation. Original test exit is retained, with all budgets unchanged.'}\n(out / 'profile-report.json').write_text(json.dumps(result, indent=2) + '\\n')\nprint(json.dumps({'test_exit': reports['head']['test_exit'], 'cores': reports['head']['daemon_cores']}), flush=True)\nsys.exit(reports['head']['test_exit'])\n"

// Performance always stays on Namespace. Main's successful runs seed durable snapshots and
// real baseline reports. Each PR can also reuse its own snapshots, outside the dependency-cache pool.
export default githubWorkflow({
  name: 'Performance mail backlog latency profile',
  on: { workflow_dispatch: { inputs: { base_sha: { type: 'string', required: true }, head_sha: { type: 'string', required: true } } } },
  permissions: { contents: 'read', actions: 'read', 'pull-requests': 'read' },
  concurrency: {
    group: 'perf-${{ github.event.pull_request.number || github.run_id }}',
    'cancel-in-progress': "${{ github.event_name == 'pull_request' }}",
  },
  actionlint: {
    ...defaultActionlintConfig,
    selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), ...linuxStageRunner],
  },
  jobs: {
    'perf-load': {
      name: 'perf-load',
      'runs-on': linuxStageRunner,
      'timeout-minutes': 30,
      defaults: { run: { shell: 'bash' } },
      env: {
        ...buildEnv,
        SCCACHE_IDLE_TIMEOUT: '0',
        PERF_PR_NUMBER: '${{ github.event.pull_request.number }}',
        PERF_HEAD_REPOSITORY: '${{ github.event.pull_request.head.repo.full_name }}',
      },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'fetch-depth': 0, 'persist-credentials': false } },
        {
          name: 'Name compatible snapshot recipes',
          env: {
            PERF_BUILD_SNAPSHOT: "perf-load-build-v1-${{ hashFiles('Cargo.lock', 'flake.lock', 'flake.nix', 'Cargo.toml', 'crates/st3/Cargo.toml', '.cargo/config.toml') }}",
            PERF_STORES_SNAPSHOT: "perf-load-stores-v1-${{ hashFiles('crates/st3/tests/daemon_bench.rs', 'docs/st3/schema.md') }}",
          },
          run: 'printf "PERF_BUILD_SNAPSHOT=%s\\nPERF_STORES_SNAPSHOT=%s\\n" "$PERF_BUILD_SNAPSHOT" "$PERF_STORES_SNAPSHOT" >> "$GITHUB_ENV"',
        },
        {
          name: 'Restore compatible build, Nix, stores and main baseline snapshots',
          env: { GH_TOKEN: '${{ github.token }}' },
          run: 'python3 scripts/ci-perf-cache restore',
        },
        ...plainFlakeSetupSteps({ nix: { binaryCaches: readOnlyBinaryCaches } }),
        {
          name: 'Isolate test state and keep the compiler cache',
          run: `bench_home="$RUNNER_TEMP/test-home"
mkdir -p "$bench_home"/{.config,.cache,.local/state} "$RUNNER_TEMP/cargo-home"/{registry,git} "$RUNNER_TEMP/perf-sccache"
printf 'CARGO_HOME=%s\\nCI_CACHE_DIR=%s\\nSCCACHE_DIR=%s\\nSCCACHE_CACHE_SIZE=1G\\n' "$RUNNER_TEMP/cargo-home" "$RUNNER_TEMP/st-ci-cache" "$RUNNER_TEMP/perf-sccache" >> "$GITHUB_ENV"
printf 'HOME=%s\\nXDG_CONFIG_HOME=%s/.config\\nXDG_CACHE_HOME=%s/.cache\\nXDG_STATE_HOME=%s/.local/state\\n' "$bench_home" "$bench_home" "$bench_home" "$bench_home" >> "$GITHUB_ENV"`,
        },
        { name: 'Use the cached Nix outputs', run: 'bash scripts/ci-nix-cache use' },
        {
          name: 'Prepare fast scratch space for a new store recipe',
          // The generator already prefers RAM when enough is available. Its source stores are
          // copied to RUNNER_TEMP before measuring: measurements still use the normal disk.
          run: 'if [ ! -s "$RUNNER_TEMP/st-bench/generated-1.sqlite3" ]; then sudo mount -o remount,size=10G /dev/shm; fi',
        },
        { ...nixDevelopStep({ name: 'Build both sources and measure main/head on one warm worker', flake: '.#perf', command: ['python3', '-c', paired] }), id: 'load', env: { PAIR_BASE: '${{ inputs.base_sha }}', PAIR_HEAD: '${{ inputs.head_sha }}' } },
        {
          name: 'Retain the load report, log and timing',
          uses: 'actions/upload-artifact@v4',
          if: 'always()',
          with: { name: 'perf-load-logs', path: '${{ runner.temp }}/perf/', 'retention-days': 30, 'if-no-files-found': 'ignore', overwrite: true },
        },
      ],
    },
  },
})
