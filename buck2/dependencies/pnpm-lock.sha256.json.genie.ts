import { createGenieOutput } from '../../repos/effect-utils/packages/@overeng/genie/src/runtime/core.ts'
import { loadBuckLockData } from './lock.ts'

const { sidecar } = await loadBuckLockData()

export default createGenieOutput({
  data: sidecar,
  stringify: () => `${JSON.stringify(sidecar, null, 2)}\n`,
})
