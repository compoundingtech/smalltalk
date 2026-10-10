import { existsSync } from 'node:fs'
import { resolve } from 'node:path'

export const appRef = { id: 'fractal-app', title: 'Fractal App', staticPath: './apps/fractal-web/storybook-static' }

export function composition({ repoRoot, appUrl, build, ci }) {
  const appBookExists = existsSync(resolve(repoRoot, 'apps/fractal-web/.storybook'))
  const output = resolve(repoRoot, 'apps/fractal-web/storybook-static')
  const appBuilt = existsSync(resolve(output, 'index.json')) && existsSync(resolve(output, 'index.html'))
  if (build && appBookExists && !appBuilt) throw new Error('The app book exists but its static output is missing. Run bash scripts/ci-fractal-web-storybooks (app first, then kit).')
  return {
    refs: {
      [appRef.id]: { title: appRef.title, url: (!build && !ci && appUrl) || appRef.staticPath, type: 'server-lazy' },
    },
    // Embed the app under the landing so publication needs only the kit output directory.
    staticDirs: build && appBookExists && appBuilt ? [{ from: output, to: '/apps/fractal-web/storybook-static' }] : [],
  }
}
