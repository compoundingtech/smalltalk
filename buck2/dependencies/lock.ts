import { existsSync, readFileSync } from 'node:fs'

import {
  decodePnpmSha256Sidecar,
  generatePnpmSha256Sidecar,
  translatePnpmLock,
  validatePnpmSha256Sidecar,
  type PnpmDependencyReference,
  type PnpmLockMetadata,
  type PnpmSha256Sidecar,
} from '../../repos/effect-utils/buck2/dependencies/pnpm-lock.ts'

/**
 * Workspace members Buck builds. The Expo app keeps its own pnpm/Metro flow, so its native and
 * bundler closure never enters the Buck graph.
 */
export const buckImporters = ['clients/typescript/st3-client', 'clients/typescript/st3-views'] as const

const root = new URL('../../', import.meta.url)
const read = (path: string): string => readFileSync(new URL(path, root), 'utf8')
export const sidecarPath = 'buck2/dependencies/pnpm-lock.sha256.json'

/** The root lock restricted to the closure of the Buck importers; its fingerprint stays the lock's. */
export const buckLockMetadata = (): PnpmLockMetadata => {
  const metadata = translatePnpmLock({
    lockfileText: read('pnpm-lock.yaml'),
    workspaceText: read('pnpm-workspace.yaml'),
  })
  const importers = new Set<string>()
  const snapshots = new Set<string>()
  const pending: PnpmDependencyReference[] = buckImporters.map((path) => ({ kind: 'workspace', path }))
  while (pending.length > 0) {
    const reference = pending.pop()!
    if (reference.kind === 'workspace') {
      if (importers.has(reference.path)) continue
      const importer = metadata.importers[reference.path]
      if (importer === undefined) throw new Error(`pnpm-lock.yaml has no importer ${reference.path}`)
      importers.add(reference.path)
      pending.push(
        ...Object.values(importer.dependencies),
        ...Object.values(importer.devDependencies),
        ...Object.values(importer.optionalDependencies),
      )
    } else {
      if (snapshots.has(reference.snapshot)) continue
      const snapshot = metadata.snapshots[reference.snapshot]
      if (snapshot === undefined) throw new Error(`pnpm-lock.yaml has no snapshot ${reference.snapshot}`)
      snapshots.add(reference.snapshot)
      pending.push(...Object.values(snapshot.dependencies), ...Object.values(snapshot.optionalDependencies))
    }
  }
  const packages = new Set([...snapshots].map((key) => metadata.snapshots[key]!.package))
  const pick = <T>(record: Readonly<Record<string, T>>, keep: (key: string, value: T) => boolean) =>
    Object.fromEntries(Object.entries(record).filter(([key, value]) => keep(key, value)))
  return {
    ...metadata,
    importers: pick(metadata.importers, (key) => importers.has(key)),
    snapshots: pick(metadata.snapshots, (key) => snapshots.has(key)),
    packages: pick(
      metadata.packages,
      (key, value) =>
        packages.has(key) || (value.resolution === 'workspace' && importers.has(value.workspacePath ?? '')),
    ),
  }
}

/**
 * Lock projection plus its archive sidecar. A fresh sidecar needs no network; a missing or stale
 * one is regenerated from the public registry, verifying each archive's lockfile sha512 first.
 */
export const loadBuckLockData = async (): Promise<{ metadata: PnpmLockMetadata; sidecar: PnpmSha256Sidecar }> => {
  const metadata = buckLockMetadata()
  let previous: PnpmSha256Sidecar | undefined
  if (existsSync(new URL(sidecarPath, root))) {
    previous = decodePnpmSha256Sidecar(JSON.parse(read(sidecarPath)))
    try {
      validatePnpmSha256Sidecar({ metadata, sidecar: previous })
      return { metadata, sidecar: previous }
    } catch {
      // Stale: regenerate below, reusing unchanged rows.
    }
  }
  const sidecar = await generatePnpmSha256Sidecar({ metadata, ...(previous === undefined ? {} : { previous }) })
  validatePnpmSha256Sidecar({ metadata, sidecar })
  return { metadata, sidecar }
}
