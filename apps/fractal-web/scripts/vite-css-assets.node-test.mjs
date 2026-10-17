import assert from 'node:assert/strict'
import { randomBytes } from 'node:crypto'
import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { build } from 'vite'
import { deferBuiltStylesheets } from './stylex.mjs'
import { fractalCssAssets } from './vite-css-assets.mjs'

// The terminal's shape: the entry lazily loads a chunk that imports its face stylesheet
// and loads the font file by URL, as terminalFonts.ts does.
const buildFixture = async (files, plugins = []) => {
  const root = await mkdtemp(join(tmpdir(), 'fractal-css-assets-'))
  try {
    const fixture = {
      'index.html': '<script type="module" src="./main.js"></script>\n',
      'main.js': "import('./terminal.js')\n",
      ...files,
    }
    for (const [name, content] of Object.entries(fixture)) await writeFile(join(root, name), content)
    const result = await build({
      root,
      configFile: false,
      logLevel: 'silent',
      plugins: [...plugins, fractalCssAssets()],
      build: { write: false },
    })
    return (Array.isArray(result) ? result : [result]).flatMap((output) => output.output)
  } finally {
    await rm(root, { recursive: true, force: true })
  }
}

// Above Vite's 4 KiB inline limit, like the real faces, so it is emitted as a file.
const font = () => randomBytes(8192)
const terminal = (fontFile) => `import './fonts.css'\nexport { default } from './${fontFile}?url'\n`
const face = (src) => `@font-face { font-family: 'Mono'; src: ${src}; }\n`

test('control: a lazy chunk whose stylesheet declares its preloaded face builds', async () => {
  const output = await buildFixture({
    'terminal.js': terminal('Mono.woff2'),
    'fonts.css': face("url('./Mono.woff2') format('woff2')"),
    'Mono.woff2': font(),
  })
  const stylesheets = output.filter((asset) => asset.fileName.endsWith('.css'))
  const emitted = output.find((asset) => asset.fileName.endsWith('.woff2'))
  assert.equal(stylesheets.length, 1)
  assert.ok(emitted)
  assert.ok(String(stylesheets[0]?.source).includes(emitted.fileName.split('/').pop() ?? ''))
})

test('control: a quoted URL with parentheses among several src alternatives declares its face', async () => {
  const output = await buildFixture({
    'terminal.js': terminal('Mono(Regular).woff2'),
    'fonts.css': face(`local('Mono'), url("./Mono(Regular).woff2") format('woff2')`),
    'Mono(Regular).woff2': font(),
  })
  assert.ok(output.some((asset) => /^assets\/Mono\(Regular\)-[\w-]+\.woff2$/.test(asset.fileName)))
})

test("control: a face declared by the entry's stylesheet covers a lazy chunk that preloads it", async () => {
  const output = await buildFixture({
    'main.js': "import './app.css'\nimport('./terminal.js')\n",
    'app.css': face("url('./Mono.woff2') format('woff2')"),
    'terminal.js': "export { default } from './Mono.woff2?url'\n",
    'Mono.woff2': font(),
  })
  assert.equal(output.filter((asset) => asset.fileName.endsWith('.css')).length, 1)
})

test('negative: a lazy chunk whose stylesheet lost every rule fails the build', async () => {
  await assert.rejects(
    buildFixture({ 'terminal.js': "import './fonts.css'\nexport default 1\n", 'fonts.css': '/* Faces moved elsewhere. */\n' }),
    /\[fractal:css-assets\]\nassets\/terminal-[\w-]+\.css has no CSS rules \(\d+ bytes\), yet assets\/terminal-[\w-]+\.js loads it/,
  )
})

test('negative: a preloaded font without a declared face fails even when other rules remain', async () => {
  await assert.rejects(
    buildFixture({
      'terminal.js': terminal('Mono.woff2'),
      'fonts.css': '::highlight(find) { color: red; }\n',
      'Mono.woff2': font(),
    }),
    /assets\/terminal-[\w-]+\.js loads assets\/Mono-[\w-]+\.woff2, but no @font-face in its stylesheets \(assets\/terminal-[\w-]+\.css\) declares it/,
  )
})

// The app document's shape: an inline themed shell, then the application stylesheet,
// whose first rule imports the kit's tokens.
const page = (link = '') => `<!doctype html><html><head><style data-wf-shell-bootstrap>html { background: #0a0a0a; }</style>${link}</head><body><script type="module" src="./main.js"></script></body></html>\n`
const appCss = (padding = '') => `@import './tokens.css';\nhtml { height: 100%; }\n${padding}`
const tokens = ':root { --canvas: #0a0a0a; }\n'

test('control: the imported app stylesheet is processed and deferred behind the shell', async () => {
  const output = await buildFixture({
    'index.html': page(),
    'main.js': "import './app.css'\n",
    'app.css': appCss(),
    'tokens.css': tokens,
  }, [deferBuiltStylesheets()])
  const html = String(output.find((asset) => asset.fileName === 'index.html')?.source)
  const stylesheet = output.find((asset) => asset.fileName.endsWith('.css'))
  assert.ok(stylesheet)
  assert.ok(!String(stylesheet.source).includes('@import'))
  assert.ok(String(stylesheet.source).includes('--canvas'))
  assert.ok(html.includes(`<link rel="stylesheet" crossorigin href="/${stylesheet.fileName}" media="print" data-wf-stylex-bootstrap>`))
  assert.ok(!html.includes('data:text/css'))
  // The shell's inline style precedes the deferred link, and the readiness bootstrap follows it.
  assert.ok(html.indexOf('data-wf-shell-bootstrap') < html.indexOf('data-wf-stylex-bootstrap'))
  assert.ok(html.indexOf('data-wf-stylex-bootstrap>') < html.indexOf("querySelectorAll('link[data-wf-stylex-bootstrap]')"))
})

test('negative: a media stylesheet link ships the raw stylesheet as a data: URL and fails the build', async () => {
  await assert.rejects(
    buildFixture({
      'index.html': page('<link rel="stylesheet" href="/app.css" media="print" />'),
      'main.js': 'export {}\n',
      'app.css': appCss(),
      'tokens.css': tokens,
    }),
    /\[fractal:css-assets\]\nindex\.html links a data: stylesheet \(data:text\/css;base64,[^)]*\), which is not a processed stylesheet of this bundle/,
  )
})

test('negative: a media stylesheet link above the inline limit ships an unprocessed @import and fails the build', async () => {
  await assert.rejects(
    buildFixture({
      'index.html': page('<link rel="stylesheet" href="/app.css" media="print" />'),
      'main.js': 'export {}\n',
      'app.css': appCss(`/* ${'x'.repeat(8192)} */\n`),
      'tokens.css': tokens,
    }),
    /assets\/app-[\w-]+\.css keeps an unprocessed @import/,
  )
})
