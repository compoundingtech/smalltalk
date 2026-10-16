import { performance } from 'node:perf_hooks'

// Storybook's preview treats a "StorybookTestRunner" user agent as a test
// environment: after the play it pauses animations at their end state for the
// a11y check instead of sleeping a fixed 100 ms first (waitForAnimations).
// Storybook's own test runner identifies itself the same way.
const testRunnerUserAgents = new WeakMap()
export function testRunnerUserAgent(browser) {
  if (!testRunnerUserAgents.has(browser)) testRunnerUserAgents.set(browser, (async () => {
    const probe = await browser.newContext()
    try {
      return `${await (await probe.newPage()).evaluate(() => navigator.userAgent)} StorybookTestRunner`
    } finally {
      await probe.close()
    }
  })())
  return testRunnerUserAgents.get(browser)
}

// Chromium's HTTP resource cache is intentionally retained: the job's server and
// bundle are immutable. Cache Storage and every other origin storage type are not.
export async function createIsolatedKitWorker(browser, base) {
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, userAgent: await testRunnerUserAgent(browser) })
  const origins = new Set()
  const trackOrigin = url => {
    const parsed = new URL(url)
    if (parsed.protocol === 'http:' || parsed.protocol === 'https:') origins.add(parsed.origin)
  }
  context.on('request', request => trackOrigin(request.url()))
  try {
    trackOrigin(base)
    const control = await context.newPage()
    const client = await context.newCDPSession(control)
    const assertControl = () => {
      if (control.isClosed() || control.url() !== 'about:blank') {
        throw new Error('Kit isolation control page must remain open and blank')
      }
    }
    const audit = async () => {
      assertControl()
      const cookies = await context.cookies()
      if (cookies.length) throw new Error(`Kit isolation audit: ${cookies.length} residual cookies`)
      for (const origin of origins) {
        const { usage } = await client.send('Storage.getUsageAndQuota', { origin })
        if (usage !== 0) throw new Error(`Kit isolation audit: residual storage at ${origin}: ${usage} bytes`)
      }
    }
    return {
      context,
      async reset() {
        const started = performance.now()
        assertControl()
        // Closing the entire previous page graph also removes sessionStorage,
        // globals, workers, in-flight scripts, and popups before clearing storage.
        let pages = context.pages().filter(page => page !== control)
        while (pages.length) {
          const results = await Promise.allSettled(pages.map(page => page.close()))
          const failures = results.filter(result => result.status === 'rejected')
          if (failures.length) throw new AggregateError(failures.map(result => result.reason), 'Kit isolation page cleanup failed')
          pages = context.pages().filter(page => page !== control)
        }
        await context.clearCookies()
        await context.clearPermissions()
        for (const origin of origins) {
          await client.send('Storage.clearDataForOrigin', { origin, storageTypes: 'all' })
        }
        await audit()
        return { resetMilliseconds: performance.now() - started }
      },
      // Audit only the FIRST HTTP(S) top-level document of each fresh page.
      // Its Web Storage snapshot is captured before app scripts, and goto
      // awaits that exact snapshot. Later within-case navigation may preserve
      // storage. Attach strictStoryGuard after newPage, before navigation.
      async newPage({ viewport, colorScheme }) {
        const page = await context.newPage()
        try {
          await page.setViewportSize(viewport)
          await page.emulateMedia({ colorScheme })
          let captureSnapshot, captured = false
          const firstSnapshot = new Promise(resolve => { captureSnapshot = resolve })
          await page.exposeBinding('__kitIsolationDocumentSnapshot', ({ frame }, snapshot) => {
            if (frame !== page.mainFrame() || captured) return
            captured = true
            captureSnapshot(snapshot)
          })
          await page.addInitScript(() => {
            if (window !== window.top || !['http:', 'https:'].includes(location.protocol)) return
            // Read synchronously at document start, not after the binding's
            // round trip, when application scripts could already mutate state.
            let snapshot
            try {
              snapshot = { origin: location.origin, local: Object.keys(localStorage), session: Object.keys(sessionStorage) }
            } catch (error) {
              snapshot = { origin: location.origin, error: String(error) }
            }
            void window.__kitIsolationDocumentSnapshot(snapshot)
          })
          const goto = page.goto.bind(page)
          page.goto = async (...args) => {
            const response = await goto(...args)
            if (captured || ['http:', 'https:'].includes(new URL(page.url()).protocol)) {
              const snapshot = await firstSnapshot
              if (snapshot.error) throw new Error(`Kit isolation document-start audit failed at ${snapshot.origin}: ${snapshot.error}`)
              if (snapshot.local.length || snapshot.session.length) {
                throw new Error(`Kit isolation document-start audit: residual Web Storage at ${snapshot.origin}`)
              }
            }
            return response
          }
          return page
        } catch (error) {
          try {
            await page.close()
          } catch (cleanupError) {
            throw new AggregateError([error, cleanupError], 'Kit isolation page setup and cleanup failed')
          }
          throw error
        }
      },
      audit,
      async close() {
        await context.close()
      },
    }
  } catch (error) {
    try {
      await context.close()
    } catch (cleanupError) {
      throw new AggregateError([error, cleanupError], 'Kit isolation worker setup and cleanup failed')
    }
    throw error
  }
}
