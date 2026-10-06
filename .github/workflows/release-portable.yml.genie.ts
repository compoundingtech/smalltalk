import { linuxActionlintConfig, linuxRunner } from './workspace-ci.ts'
import { buildSnapshotPrepare, buildSnapshotRestore, buildSnapshotSave } from './build-snapshot.ts'
import { auditCaches } from './cache-audit.ts'
import { githubWorkflow } from '../../repos/effect-utils/genie/external.ts'

// Preserve release triggers, source verification and publishing permissions.
export default githubWorkflow(auditCaches({
  actionlint: linuxActionlintConfig,
  "name": "Publish portable st2 source",
  "on": {
    "workflow_dispatch": {
      "inputs": {
        "source_sha": {
          "description": "Exact accepted commit SHA to build",
          "required": true,
          "type": "string"
        },
        "tag": {
          "description": "Successor release tag",
          "required": true,
          "type": "string"
        }
      }
    }
  },
  "permissions": {
    "contents": "write",
    "actions": "read"
  },
  "jobs": {
    "linux-x86_64": {
      "runs-on": linuxRunner,
      "timeout-minutes": 30,
      "env": {
        "SOURCE_SHA": "${{ inputs.source_sha }}",
        CI_NATIVE_SNAPSHOT: '1',
        "TAG": "${{ inputs.tag }}"
      },
      "steps": [
        {
          uses: 'actions/checkout@v4',
          with: { ref: '${{ github.workflow_sha }}', 'persist-credentials': false },
        },
        {
          name: 'Keep the immutable workflow helpers before selecting accepted source',
          run: `mkdir -p "$RUNNER_TEMP/ci-tools"
cp scripts/ci-cache-audit scripts/ci-build-snapshot scripts/ci-perf-cache "$RUNNER_TEMP/ci-tools/"
cp .github/workflows/release-portable.yml "$RUNNER_TEMP/ci-tools/pipeline.yml"
printf 'CI_SNAPSHOT_PIPELINE_FILE=%s/ci-tools/pipeline.yml\\n' "$RUNNER_TEMP" >> "$GITHUB_ENV"
printf 'CI_CACHE_AUDIT_SCRIPT=%s/ci-cache-audit\\nCI_BUILD_SNAPSHOT_SCRIPT=%s/ci-build-snapshot\\n' "$RUNNER_TEMP/ci-tools" "$RUNNER_TEMP/ci-tools" >> "$GITHUB_ENV"`,
        },
        {
          "uses": "actions/checkout@v4",
          "with": {
            "ref": "${{ inputs.source_sha }}",
            "fetch-depth": 0
          }
        },
        {
          "uses": "dtolnay/rust-toolchain@stable"
        },
        buildSnapshotRestore,
        {
          if: "env.CI_BUILD_SNAPSHOT_HIT != '1'",
          name: 'Restore portable Cargo dependencies',
          uses: 'Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6',
          with: { key: 'portable-linux-x86-64', 'cache-on-failure': true },
        },
        {
          "name": "Verify accepted immutable source",
          "run": "set -euo pipefail\ntest -n \"$SOURCE_SHA\"\ntest \"$(git rev-parse HEAD)\" = \"$SOURCE_SHA\"\ngit merge-base --is-ancestor \"$SOURCE_SHA\" origin/main\ntest -z \"$(git status --porcelain)\"\nSHORT_SHA=\"$(git rev-parse --short=7 HEAD)\"\necho \"SHORT_SHA=$SHORT_SHA\" >> \"$GITHUB_ENV\"\n"
        },
        buildSnapshotPrepare,
        {
          "name": "Build and verify portable binary",
          "run": "set -euo pipefail\nmkdir -p \"$RUNNER_TEMP/portable-proof\"\nexport CARGO_ENCODED_RUSTFLAGS=\nif [ \"${CI_BUILD_SNAPSHOT_HIT:-}\" != 1 ]; then cargo build --release --locked; fi\n./target/release/st2 --version | tee \"$RUNNER_TEMP/portable-proof/st2.version.txt\"\ngrep -F \"$SHORT_SHA\" \"$RUNNER_TEMP/portable-proof/st2.version.txt\"\nfile target/release/st2 | tee \"$RUNNER_TEMP/portable-proof/st2.file.txt\"\ngrep -F \"ELF 64-bit LSB\" \"$RUNNER_TEMP/portable-proof/st2.file.txt\"\nreadelf --program-headers target/release/st2 | tee \"$RUNNER_TEMP/portable-proof/st2.program-headers.txt\"\n! grep -F \"/nix/store/\" \"$RUNNER_TEMP/portable-proof/st2.program-headers.txt\"\ngrep -E \"Requesting program interpreter: /(lib64|lib/x86_64-linux-gnu)/\" \"$RUNNER_TEMP/portable-proof/st2.program-headers.txt\"\nldd target/release/st2 | tee \"$RUNNER_TEMP/portable-proof/st2.ldd.txt\"\n! grep -F \"not found\" \"$RUNNER_TEMP/portable-proof/st2.ldd.txt\"\n"
        },
        ...buildSnapshotSave,
        {
          "name": "Package and checksum",
          "run": "set -euo pipefail\nARCHIVE=\"st2-${TAG#v}-x86_64-unknown-linux-gnu.tar.gz\"\ninstall -m 0755 target/release/st2 st2\ntar -czf \"$ARCHIVE\" st2\nsha256sum \"$ARCHIVE\" > SHA256SUMS\ntest \"$(tar -tzf \"$ARCHIVE\")\" = st2\necho \"ARCHIVE=$ARCHIVE\" >> \"$GITHUB_ENV\"\n"
        },
        {
          "name": "Publish successor release",
          "env": {
            "GH_TOKEN": "${{ github.token }}"
          },
          "run": "set -euo pipefail\ngit fetch --tags origin\nif git rev-parse \"refs/tags/$TAG\" >/dev/null 2>&1; then\n  test \"$(git rev-list -n 1 \"$TAG\")\" = \"$SOURCE_SHA\"\nelse\n  git config user.name github-actions\n  git config user.email github-actions@github.com\n  git tag -a \"$TAG\" \"$SOURCE_SHA\" -m \"st2 $TAG\"\n  git push origin \"refs/tags/$TAG\"\nfi\nprintf 'Exact source: `%s`.\\n\\nPortable Linux x86_64 artifact with checksum and fresh-download execution proof.\\n' \"$SOURCE_SHA\" > release-notes.md\ngh release create \"$TAG\" --verify-tag --target \"$SOURCE_SHA\" --prerelease --title \"st2 $TAG\" --notes-file release-notes.md -- \"$ARCHIVE\" SHA256SUMS\n"
        },
        {
          "name": "Verify fresh download and execution",
          "env": {
            "GH_TOKEN": "${{ github.token }}"
          },
          "run": "set -euo pipefail\nmkdir verified-download\ngh release download \"$TAG\" --dir verified-download --pattern \"$ARCHIVE\" --pattern SHA256SUMS\ncd verified-download\nsha256sum --check SHA256SUMS\ntar -xzf \"$ARCHIVE\"\n./st2 --version | tee verified.version.txt\ngrep -F \"$SHORT_SHA\" verified.version.txt\n"
        }
      ]
    }
  }
}, {}))
