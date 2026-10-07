export const buildSnapshotRestore = {
  id: 'build-snapshot', name: 'Restore the exact-source build snapshot',
  if: "env.CI_LOCAL_CACHES != '1'",
  env: { GH_TOKEN: '${{ github.token }}', CI_SNAPSHOT_PIPELINE: '${{ github.workflow_sha }}' },
  run: 'python3 "${CI_BUILD_SNAPSHOT_SCRIPT:-scripts/ci-build-snapshot}" restore',
}
export const buildSnapshotPrepare = {
  name: 'Remember genuine source inputs before compiling',
  run: 'python3 "${CI_BUILD_SNAPSHOT_SCRIPT:-scripts/ci-build-snapshot}" prepare',
}
export const buildSnapshotSave = [
  {
    id: 'build-snapshot-save', name: 'Keep valid same-source build outputs', if: '!cancelled()',
    env: { CI_SNAPSHOT_PIPELINE: '${{ github.workflow_sha }}' },
    run: 'python3 "${CI_BUILD_SNAPSHOT_SCRIPT:-scripts/ci-build-snapshot}" pack',
  },
  {
    id: 'build-snapshot-upload',
    name: 'Retain the same-source build snapshot',
    if: "!cancelled() && steps.build-snapshot-save.outputs.publish == 'true'",
    'continue-on-error': true,
    'timeout-minutes': 5,
    uses: 'actions/upload-artifact@v4',
    with: { name: '${{ steps.build-snapshot-save.outputs.name }}', path: '${{ runner.temp }}/ci-build-snapshot/build.tar.zst',
      'compression-level': 0, 'retention-days': 3, 'if-no-files-found': 'error', overwrite: true },
  },
  {
    name: 'Wait before retrying build snapshot retention',
    if: "!cancelled() && steps.build-snapshot-upload.outcome == 'failure'",
    'timeout-minutes': 1,
    shell: 'bash',
    run: 'echo "::warning::Build snapshot upload failed; retrying once in 15 seconds."; sleep 15',
  },
  {
    name: 'Retry retaining the same-source build snapshot',
    if: "!cancelled() && steps.build-snapshot-upload.outcome == 'failure'",
    'timeout-minutes': 5,
    uses: 'actions/upload-artifact@v4',
    with: { name: '${{ steps.build-snapshot-save.outputs.name }}', path: '${{ runner.temp }}/ci-build-snapshot/build.tar.zst',
      'compression-level': 0, 'retention-days': 3, 'if-no-files-found': 'error', overwrite: true },
  },
] as const
