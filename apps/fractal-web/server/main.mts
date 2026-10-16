// The production entry for a tailnet-hosted web app; the environment is documented in docs/fractal-web.md.
import { readHostConfig, startHost } from './host.mts'

const served = startHost(readHostConfig(process.env))
served.listen()
served.server.once('listening', () => console.log('fractal-web listening', served.server.address()))
const stop = () => { void served.close().then(() => process.exit(0)) }
process.once('SIGTERM', stop)
process.once('SIGINT', stop)
