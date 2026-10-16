import { readyPRJobs, readyPullRequestTypes } from './ready-pr.ts'
import { linuxActionlintConfig, readOnlyBinaryCaches, priorityRunnerJob } from './workspace-ci.ts'
import { buildSnapshotPrepare, buildSnapshotRestore, buildSnapshotSave } from './build-snapshot.ts'
import { auditCaches } from './cache-audit.ts'
import { githubWorkflow, plainFlakeSetupSteps } from '../../repos/effect-utils/genie/external.ts'
import { readFileSync } from 'node:fs'

const releaseRustVersion = readFileSync(new URL('../../scripts/release-rust-version', import.meta.url), 'utf8').trim()

// Preserve release triggers, source verification and publishing permissions. Every commit on main
// also builds and verifies both archives and keeps them as short-lived artifacts, so release
// breakage fails on main instead of at tag time (the daily release publishes those artifacts).
// After a green main run, one rolling `edge` pre-release is moved to that commit. It is excluded from
// the tag trigger above, and a tag made with the workflow token starts no workflow in any case.
export default githubWorkflow(readyPRJobs(auditCaches({
  actionlint: linuxActionlintConfig,
  "name": "Smalltalk tag release",
  "on": {
    "push": {
      "branches": [
        "main"
      ],
      "tags": [
        "**",
        "!edge"
      ]
    },
    "workflow_dispatch": null,
    "pull_request": {
      types: readyPullRequestTypes,
      "paths": [
        ".github/workflows/release-smalltalk.yml",
        ".github/workflows/release-smalltalk.yml.genie.ts",
        ".gitignore",
        "crates/st-drivers/build.rs",
        "crates/st-drivers/src/version.rs",
        "install.sh",
        "scripts/download-install-test",
        "scripts/linux-release-cc",
        "scripts/check-linux-release.py",
        "scripts/release-build-stamp.py",
        "scripts/release-rust-version",
        "scripts/fixtures/install-release-v0.3.17",
        "scripts/release-smalltalk*",
        "scripts/release_notes*",
        "scripts/compat-baseline-release*",
        ".github/*-compat-baseline.json",
        "scripts/ci-build-snapshot*",
        "scripts/ci-cache-audit*",
        "scripts/ci-perf-cache",
        ".github/workflows/build-snapshot.ts",
        "scripts/install-release*",
        "scripts/install-macos*",
        "docs/st3/binary-releases.md",
        ".github/workflows/workspace-ci.ts",
        "scripts/ci-pick-runner",
        "flake.lock"
      ]
    }
  },
  "permissions": {
    "contents": "read",
    "actions": "read"
  },
  "concurrency": {
    "group": "smalltalk-release-${{ github.event_name == 'pull_request' && github.ref || (github.event_name == 'push' && github.ref == 'refs/heads/main' && 'main') || github.run_id }}",
    "cancel-in-progress": "${{ github.event_name == 'pull_request' }}"
  },
  "jobs": {
    "pick-runner": priorityRunnerJob,
    "installer": {
      "name": "installer (${{ matrix.runner }})",
      "strategy": {
        "fail-fast": false,
        "matrix": {
          "runner": [
            'ubuntu-latest',
            'macos-15'
          ]
        }
      },
      "runs-on": "${{ matrix.runner }}",
      "timeout-minutes": 5,
      "steps": [
        {
          "uses": "actions/checkout@v4",
          "with": {
            "persist-credentials": false
          }
        },
        {
          "name": "Test install entry points and isolated app transactions",
          "run": "scripts/install-test\nscripts/install-release-test\npython3 scripts/install-macos-test\npython3 scripts/release_notes_test.py\npython3 scripts/release-smalltalk-edge-test\npython3 scripts/compat-baseline-release-test\n"
        },
        {
          "name": "Test the daily release failure path",
          "run": "python3 scripts/release-smalltalk-daily-test"
        }
      ]
    },
    "build": {
      "needs": ["pick-runner"],
      "if": "${{ !cancelled() && (github.event_name == 'workflow_dispatch' || (github.event_name == 'push' && github.event.deleted == false) || github.event_name == 'pull_request') }}",
      "name": "release-build (${{ matrix.target }})",
      "strategy": {
        "fail-fast": false,
        "matrix": {
          "include": [
            {
              "runner": 'namespace-profile-linux-x86-64;job.priority=2;github.run-id=${{ github.run_id }}',
              "target": "x86_64-unknown-linux-gnu"
            },
            {
              "runner": "macos-15",
              "target": "aarch64-apple-darwin"
            }
          ]
        }
      },
      "runs-on": "${{ matrix.target == 'x86_64-unknown-linux-gnu' && github.event_name != 'pull_request' && (needs['pick-runner'].outputs.ci1 && fromJSON(needs['pick-runner'].outputs.ci1)[0] || 'ci1-priority') || matrix.runner }}",
      "timeout-minutes": 90,
      "env": {
        "MACOSX_DEPLOYMENT_TARGET": "15.0",
        CI_NATIVE_SNAPSHOT: '1',
        "RELEASE_TAG": "${{ github.ref_type == 'tag' && github.ref_name || '' }}"
      },
      "steps": [
        {
          "uses": "actions/checkout@v4",
          "with": {
            "persist-credentials": false
          }
        },
        {
          "uses": "dtolnay/rust-toolchain@7e38f4b43b4db5c8dd498af069a4f6196df1d067",
          "with": { "toolchain": releaseRustVersion }
        },
        { name: 'Name the pinned compiler cache', run: `printf 'CI_ZIG_CACHE_DIR=%s/zig/0.15.2\\n' \"$RUNNER_TOOL_CACHE\" >> \"$GITHUB_ENV\"` },
        buildSnapshotRestore,
        {
          if: "env.CI_BUILD_SNAPSHOT_HIT != '1'",
          "uses": "Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6",
          "with": {
            "key": "release-${{ matrix.target }}",
            "cache-on-failure": true
          }
        },
        {
          name: 'Restore the pinned Zig compiler',
          if: "env.CI_BUILD_SNAPSHOT_ZIG_HIT != '1'",
          uses: 'actions/cache@v4',
          with: {
            path: '${{ env.CI_ZIG_CACHE_DIR }}',
            key: 'zig-tool-v1-${{ runner.os }}-${{ runner.arch }}-0.15.2',
          },
        },
        {
          "uses": "mlugg/setup-zig@d1434d08867e3ee9daa34448df10607b98908d29",
          "with": {
            "version": "0.15.2",
            "use-cache": false
          }
        },
        buildSnapshotPrepare,
        ...plainFlakeSetupSteps({ nix: { binaryCaches: readOnlyBinaryCaches } }).map(step => ({ ...step, if: "runner.os == 'Linux'" })),
        {
          name: 'Provide the pinned Linux host C compiler',
          if: "runner.os == 'Linux'",
          run: `set -euo pipefail
compiler=$(nix build .#release-cc^out --no-link --print-out-paths)
"$compiler/bin/cc" --version
printf '%s/bin\n' "$compiler" >> "$GITHUB_PATH"`,
        },
        {
          "name": "Test installer",
          "run": "scripts/install-release-test\npython3 scripts/install-macos-test\npython3 scripts/release-smalltalk-test\npython3 scripts/release_notes_test.py\n"
        },
        {
          "name": "Build, package, and test extracted tools",
          "run": "scripts/release-smalltalk '${{ matrix.target }}' dist"
        },
        ...buildSnapshotSave,
        {
          "uses": "actions/upload-artifact@v4",
          "with": {
            "name": "release-${{ matrix.target }}",
            "path": "dist/*",
            "if-no-files-found": "error",
            "retention-days": 7
          }
        }
      ]
    },
    "assemble": {
      "name": "release-assemble",
      "needs": [
        "build",
        "installer"
      ],
      "runs-on": "ubuntu-latest",
      "steps": [
        {
          "uses": "actions/checkout@v4",
          "with": {
            "persist-credentials": false
          }
        },
        {
          "uses": "actions/download-artifact@v4",
          "with": {
            "pattern": "release-*",
            "merge-multiple": true,
            "path": "dist"
          }
        },
        {
          "name": "Verify both targets and record exact sources",
          "env": {
            "SOURCE_SHA": "${{ github.sha }}"
          },
          "run": "set -euo pipefail\ncd dist\ncat ./*.sha256 > SHA256SUMS\nsha256sum --check SHA256SUMS\npython3 ../scripts/release-smalltalk-manifest.py \"$SOURCE_SHA\"\ncp ../docs/st3/binary-releases.md RELEASE-NOTES.md\nprintf '\\nExact source: `%s`. See RELEASE.json for the PTY revision and targets.\\n' \"$SOURCE_SHA\" >> RELEASE-NOTES.md\n"
        },
        {
          "uses": "actions/upload-artifact@v4",
          "with": {
            "name": "smalltalk-release",
            "path": "dist/*",
            "if-no-files-found": "error",
            "retention-days": 7
          }
        }
      ]
    },
    "publishEdge": {
      "name": "release-publish-edge",
      "if": "github.event_name == 'push' && github.ref == 'refs/heads/main'",
      "needs": "assemble",
      "runs-on": "ubuntu-latest",
      "timeout-minutes": 15,
      // One edge at a time; a superseded pending run is dropped, and the script never moves edge backwards.
      "concurrency": {
        "group": "smalltalk-edge-publish",
        "cancel-in-progress": false
      },
      "permissions": {
        "contents": "write"
      },
      "env": {
        "GH_TOKEN": "${{ github.token }}",
        "GH_REPO": "${{ github.repository }}",
        "SOURCE_SHA": "${{ github.sha }}",
        "RUN_URL": "${{ github.server_url }}/${{ github.repository }}/actions/runs/${{ github.run_id }}"
      },
      "steps": [
        {
          "uses": "actions/checkout@v4",
          "with": {
            "fetch-depth": 0,
            "persist-credentials": false
          }
        },
        {
          "uses": "actions/download-artifact@v4",
          "with": {
            "name": "smalltalk-release",
            "path": "dist"
          }
        },
        {
          "name": "Move the rolling edge pre-release to this verified commit",
          "run": "scripts/release-smalltalk-edge --source \"$SOURCE_SHA\" --dist dist --run-url \"$RUN_URL\""
        }
      ]
    },
    "publish": {
      "name": "release-publish",
      "if": "github.event_name == 'push' && startsWith(github.ref, 'refs/tags/') && github.event.deleted == false",
      "needs": "assemble",
      "runs-on": "ubuntu-latest",
      "permissions": {
        "contents": "write"
      },
      "env": {
        "GH_TOKEN": "${{ github.token }}",
        "GH_REPO": "${{ github.repository }}",
        "TAG": "${{ github.ref_name }}",
        "SOURCE_SHA": "${{ github.sha }}"
      },
      "steps": [
        {
          "uses": "actions/checkout@v4",
          "with": {
            "fetch-depth": 0
          }
        },
        {
          "uses": "actions/download-artifact@v4",
          "with": {
            "name": "smalltalk-release",
            "path": "dist"
          }
        },
        {
          "name": "Publish the verified tag artifacts",
          "run": "set -euo pipefail\ngit fetch origin \"refs/tags/$TAG\"\ntest \"$(git rev-parse 'FETCH_HEAD^{commit}')\" = \"$SOURCE_SHA\"\npython3 scripts/compat-baseline-release check --source \"$SOURCE_SHA\" --require-latest\npython3 scripts/release_notes.py --source \"$SOURCE_SHA\" --tag \"$TAG\" --output dist/RELEASE-NOTES.md --manifest dist/UPGRADE-IMPACT.json\ncd dist\nsha256sum --check SHA256SUMS\npython3 ../scripts/release-smalltalk-manifest.py \"$SOURCE_SHA\"\n# Refuse to replace any existing release or its assets. A partial draft stays private\n# for inspection; delete that draft before retrying, never move the tag.\ngh release create \"$TAG\" --verify-tag --draft --title \"Smalltalk $TAG\" \\\n  --notes-file RELEASE-NOTES.md ./*.tar.gz ./*.sha256 SHA256SUMS RELEASE.json UPGRADE-IMPACT.json\ngh release edit \"$TAG\" --draft=false\n"
        }
      ]
    },
    advance: {
      name: 'advance-compatibility-baselines',
      if: "github.event_name == 'push' && startsWith(github.ref, 'refs/tags/') && github.event.deleted == false",
      needs: 'publish',
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 20,
      permissions: { contents: 'write', actions: 'write', 'pull-requests': 'write' },
      env: {
        GH_TOKEN: '${{ github.token }}',
        GH_REPO: '${{ github.repository }}',
        TAG: '${{ github.ref_name }}',
      },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        {
          name: 'Propose compatibility baselines after publication',
          run: 'python3 scripts/compat-baseline-release advance "$TAG"',
        },
      ],
    }
  }
}, {"pick-runner": "Only reads organization runner metadata; checks out and builds nothing.", "installer": "Runs isolated installer fixtures without downloads or compilation.", "assemble": "Downloads verified build artifacts; builds nothing.", "publishEdge": "Moves the rolling edge pre-release to a verified main commit; builds nothing.", "publish": "Publishes verified assembled artifacts; builds nothing.", "advance": "Verifies published archives and proposes the baseline maintenance PR; builds nothing."})))
