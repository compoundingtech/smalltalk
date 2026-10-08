import * as React from 'react'
import { createRoot } from 'react-dom/client'
import { flushSync } from 'react-dom'
import { Tracer } from 'effect'
import { makeUxTelemetry } from './ux.ts'

// Executable browser fixture: the managed browser runner asserts body[data-test-result=pass].
const ended: Tracer.Span[] = []
let committed = false
let afterPaint = false
const tracer = Tracer.make({ span: (options) => {
  const span = Tracer.nativeTracer.span(options)
  const end = span.end.bind(span)
  span.end = (time, exit) => {
    end(time, exit)
    if (span.name === 'wf.ux.switch' && span.attributes.get('wf.ux.outcome') === 'painted') {
      afterPaint = committed && document.querySelector('[data-transcript]')?.textContent === 'Verified transcript'
    }
    ended.push(span)
  }
  return span
} })
const ux = makeUxTelemetry({ tracer: () => tracer })
const container = document.getElementById('root')!
const root = createRoot(container)
const Transcript = () => <article data-transcript ref={(node) => {
  if (node === null) return
  committed = true
  return ux.transcriptCommitted('agent/example')
}}>Verified transcript</article>
try {
  ux.beginSwitch({ ref: 'agent/example', warm: false, slotCount: 1 })
  ux.switchDataReady('agent/example')
  if (ended.some((span) => span.name === 'wf.ux.switch')) throw new Error('Data arrival incorrectly completed switch')
  flushSync(() => root.render(<React.StrictMode><Transcript /></React.StrictMode>))
  if (ended.some((span) => span.name === 'wf.ux.switch')) throw new Error('DOM commit incorrectly completed switch before paint')
  const check = () => {
    const span = ended.find((value) => value.name === 'wf.ux.switch')
    if (span === undefined) { requestAnimationFrame(check); return }
    const pass = afterPaint && span.attributes.get('wf.ux.painted') === true && span.attributes.get('span.label') === 'cold'
    document.body.dataset.testResult = pass ? 'pass' : 'fail'
    document.body.dataset.proof = JSON.stringify({ committed, afterPaint, outcome: span.attributes.get('wf.ux.outcome'), name: span.name, label: span.attributes.get('span.label') })
    flushSync(() => root.unmount())
    ux.dispose()
  }
  requestAnimationFrame(check)
} catch (error) {
  document.body.dataset.testResult = 'fail'
  document.body.dataset.proof = String(error)
  root.unmount()
  ux.dispose()
}
