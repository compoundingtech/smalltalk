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
    name: 'Retain the same-source build snapshot',
    if: "!cancelled() && steps.build-snapshot-save.outputs.publish == 'true'",
    uses: 'actions/upload-artifact@v4',
    with: { name: '${{ steps.build-snapshot-save.outputs.name }}', path: '${{ runner.temp }}/ci-build-snapshot/build.tar.zst',
      'compression-level': 0, 'retention-days': 3, 'if-no-files-found': 'error', overwrite: true },
  },
] as const
