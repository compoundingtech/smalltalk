import { defaultActionlintConfig, githubWorkflow } from '../../repos/effect-utils/genie/external.ts'

// Preserve release triggers, source verification and publishing permissions.
export default githubWorkflow({
  actionlint: defaultActionlintConfig,
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
    "contents": "write"
  },
  "jobs": {
    "linux-x86_64": {
      "runs-on": "namespace-profile-linux-x86-64",
      "timeout-minutes": 30,
      "env": {
        "SOURCE_SHA": "${{ inputs.source_sha }}",
        "TAG": "${{ inputs.tag }}"
      },
      "steps": [
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
        {
          "name": "Verify accepted immutable source",
          "run": "set -euo pipefail\ntest -n \"$SOURCE_SHA\"\ntest \"$(git rev-parse HEAD)\" = \"$SOURCE_SHA\"\ngit merge-base --is-ancestor \"$SOURCE_SHA\" origin/main\ntest -z \"$(git status --porcelain)\"\nSHORT_SHA=\"$(git rev-parse --short=7 HEAD)\"\necho \"SHORT_SHA=$SHORT_SHA\" >> \"$GITHUB_ENV\"\n"
        },
        {
          "name": "Build and verify portable binary",
          "run": "set -euo pipefail\ncargo build --release --locked\n./target/release/st2 --version | tee st2.version.txt\ngrep -F \"$SHORT_SHA\" st2.version.txt\nfile target/release/st2 | tee st2.file.txt\ngrep -F \"ELF 64-bit LSB\" st2.file.txt\nreadelf --program-headers target/release/st2 | tee st2.program-headers.txt\n! grep -F \"/nix/store/\" st2.program-headers.txt\ngrep -E \"Requesting program interpreter: /(lib64|lib/x86_64-linux-gnu)/\" st2.program-headers.txt\nldd target/release/st2 | tee st2.ldd.txt\n! grep -F \"not found\" st2.ldd.txt\n"
        },
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
})
