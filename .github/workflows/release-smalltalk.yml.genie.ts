import { buildSnapshotPrepare, buildSnapshotRestore, buildSnapshotSave } from './build-snapshot.ts'
import { auditCaches } from './cache-audit.ts'
import { defaultActionlintConfig, githubWorkflow } from '../../repos/effect-utils/genie/external.ts'

// Preserve release triggers, source verification and publishing permissions. Every commit on main
// also builds and verifies both archives and keeps them as short-lived artifacts, so release
// breakage fails on main instead of at tag time (the daily release publishes those artifacts).
export default githubWorkflow(auditCaches({
  actionlint: defaultActionlintConfig,
  "name": "Smalltalk tag release",
  "on": {
    "push": {
      "branches": [
        "main"
      ],
      "tags": [
        "**"
      ]
    },
    "workflow_dispatch": null,
    "pull_request": {
      "paths": [
        ".github/workflows/release-smalltalk.yml",
        ".github/workflows/release-smalltalk.yml.genie.ts",
        ".gitignore",
        "build.rs",
        "crates/st-drivers/src/version.rs",
        "scripts/release-smalltalk*",
        "scripts/ci-build-snapshot*",
        "scripts/ci-cache-audit*",
        "scripts/ci-perf-cache",
        ".github/workflows/build-snapshot.ts",
        "scripts/install-release*",
        "scripts/install-macos*",
        "docs/st3/binary-releases.md",
        "flake.lock"
      ]
    }
  },
  "permissions": {
    "contents": "read",
    "actions": "read"
  },
  "concurrency": {
    "group": "smalltalk-release-${{ github.event_name == 'pull_request' && github.ref || github.run_id }}",
    "cancel-in-progress": "${{ github.event_name == 'pull_request' }}"
  },
  "jobs": {
    "installer": {
      "name": "installer (${{ matrix.runner }})",
      "strategy": {
        "fail-fast": false,
        "matrix": {
          "runner": [
            "namespace-profile-linux-x86-64",
            "namespace-profile-macos-arm64"
          ]
        }
      },
      "runs-on": [
        "${{ matrix.runner }}",
        "namespace-features:github.run-id=${{ github.run_id }}"
      ],
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
          "run": "scripts/install-test\nscripts/install-release-test\npython3 scripts/install-macos-test\n"
        }
      ]
    },
    "build": {
      "if": "${{ github.event_name == 'workflow_dispatch' || (github.event_name == 'push' && github.event.deleted == false) || github.event_name == 'pull_request' }}",
      "name": "release-build (${{ matrix.target }})",
      "strategy": {
        "fail-fast": false,
        "matrix": {
          "include": [
            {
              "runner": "namespace-profile-linux-x86-64",
              "target": "x86_64-unknown-linux-gnu"
            },
            {
              "runner": "namespace-profile-macos-arm64",
              "target": "aarch64-apple-darwin"
            }
          ]
        }
      },
      "runs-on": [
        "${{ matrix.runner }}",
        "namespace-features:github.run-id=${{ github.run_id }}"
      ],
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
          "uses": "dtolnay/rust-toolchain@stable"
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
          name: 'Restore the stable Zig object cache',
          uses: 'actions/cache@v4',
          with: {
            path: '~/.cache/zig\n~/Library/Caches/zig\n.zig-cache',
            key: "zig-v1-${{ runner.os }}-${{ runner.arch }}-${{ matrix.target }}-0.15.2-${{ hashFiles('Cargo.lock', 'flake.lock') }}",
            'restore-keys': 'zig-v1-${{ runner.os }}-${{ runner.arch }}-${{ matrix.target }}-0.15.2-',
          },
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
        {
          "name": "Test installer",
          "run": "scripts/install-release-test\npython3 scripts/install-macos-test\npython3 scripts/release-smalltalk-test\n"
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
      "runs-on": "namespace-profile-linux-x86-64",
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
    "publish": {
      "name": "release-publish",
      "if": "github.event_name == 'push' && startsWith(github.ref, 'refs/tags/') && github.event.deleted == false",
      "needs": "assemble",
      "runs-on": "namespace-profile-linux-x86-64",
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
          "run": "set -euo pipefail\ngit fetch origin \"refs/tags/$TAG\"\ntest \"$(git rev-parse 'FETCH_HEAD^{commit}')\" = \"$SOURCE_SHA\"\ncd dist\nsha256sum --check SHA256SUMS\npython3 ../scripts/release-smalltalk-manifest.py \"$SOURCE_SHA\"\n# Refuse to replace any existing release or its assets. A partial draft stays private\n# for inspection; delete that draft before retrying, never move the tag.\ngh release create \"$TAG\" --verify-tag --draft --title \"Smalltalk $TAG\" \\\n  --notes-file RELEASE-NOTES.md ./*.tar.gz ./*.sha256 SHA256SUMS RELEASE.json\ngh release edit \"$TAG\" --draft=false\n"
        }
      ]
    }
  }
}, {"installer": "Runs isolated installer fixtures without downloads or compilation.", "assemble": "Downloads verified build artifacts; builds nothing.", "publish": "Publishes verified assembled artifacts; builds nothing."}))
