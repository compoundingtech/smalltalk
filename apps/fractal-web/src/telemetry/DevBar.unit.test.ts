import { describe, expect, it } from 'vitest'

import appSource from '../web/App.tsx?raw'
import devbarSource from './DevBar.tsx?raw'

/** These source-boundary guards run without importing browser diagnostics into Node. */
describe('composable developer diagnostics boundary', () => {
  it('loads diagnostic modules only through a statically gated dynamic import', () => {
    // A static DevBar import in App used to pull the diagnostic graph into production.
    expect(appSource).not.toMatch(/import\s+\{\s*DevBar\s*\}\s+from/)
    expect(appSource).toMatch(/import\.meta\.env\.DEV\s*\|\|\s*import\.meta\.env\.PERF\s*\?\s*React\.lazy/)
    expect(appSource).toContain("import('../telemetry/DevBar.tsx')")
  })

  it('composes the kit shell with host-owned counters, meters, and transport slots', () => {
    expect(devbarSource).toContain("import { Devbar } from '@overeng/devbar'")
    expect(devbarSource).toContain("{ id: 'counters', label: 'Counters'")
    expect(devbarSource).toContain("{ id: 'meters', label: 'Meters'")
    for (const id of ['version', 'websocket', 'http', 'subscriptions', 'message-rate']) {
      expect(devbarSource).toContain(`{ id: '${id}', render:`)
    }
    expect(devbarSource).toContain('<MetersProvider meters={meters}>')
    expect(devbarSource).toContain('openPanel={openPanel} onOpenPanelChange={setOpenPanel}')
    expect(devbarSource).toContain("key: 'devbar.visible'")
  })
})
