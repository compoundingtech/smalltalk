import { mkdir, writeFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createServer } from 'vite'
import stylex from '@stylexjs/unplugin'

const root = fileURLToPath(new URL('..', import.meta.url))
const receipt = process.argv[2]
const server = await createServer({
  root,
  configFile: false,
  plugins: [stylex.vite({ useCSSLayers: false })],
  server: { middlewareMode: true },
  ssr: { noExternal: ['@smalltalk/st3-scenarios', '@smalltalk/st3-client'] },
})
try {
  const { scenarioStoryCheck } = await server.ssrLoadModule('@smalltalk/st3-scenarios/storybook')
  const checks = []
  for (const path of ['src/assistant-ui/sidebar/ScenarioRoster.stories.tsx', 'src/assistant-ui/EmbraceThread.stories.tsx', 'src/assistant-ui/st3-views/SyncLine.stories.tsx']) {
    const stories = await server.ssrLoadModule(resolve(root, path))
    for (const name of ['Explore', 'Pinned']) {
      const result = scenarioStoryCheck(stories[name], stories.default, {
        scenarioNow: '2030-01-01T12:00:00.000Z',
        atMs: 10000,
        contrastPairs: { sync: [['live', 'socket-dropped'], ['socket-dropped', 'reconnected'], ['live', 'subscription-limit-local']] },
      })
      checks.push({ story: path, name, ...result, reads: [...result.reads] })
    }
  }
  const result = { passed: checks.every(check => check._tag === 'Passed'), checks }
  const json = JSON.stringify(result, null, 2) + '\n'
  console.log(json)
  if (receipt !== undefined) {
    await mkdir(resolve(receipt, '..'), { recursive: true })
    await writeFile(receipt, json)
  }
  if (!result.passed) process.exitCode = 1
} finally {
  await server.close()
}
