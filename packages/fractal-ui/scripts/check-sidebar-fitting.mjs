// Exercise the actual DOM-fitting functions against deterministic grid geometry without a browser bootstrap.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import ts from 'typescript'

const sourcePath = process.argv[2] ?? fileURLToPath(new URL('../src/assistant-ui/sidebar/SidebarRowSignals.tsx', import.meta.url))
const source = readFileSync(sourcePath, 'utf8')
const ast = ts.createSourceFile(sourcePath, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX)
const names = new Set(['writeStyle', 'fitLine1', 'measureAndApplyTracks', 'restoreLine1Fields'])
const functions = ast.statements.filter(statement => (ts.isFunctionDeclaration(statement) && names.has(statement.name?.text)) || (ts.isVariableStatement(statement) && statement.declarationList.declarations.some(declaration => names.has(declaration.name.getText(ast)))))
const javascript = ts.transpileModule(functions.map(statement => statement.getText(ast)).join('\n'), { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText
const { fit, restore } = new Function('getComputedStyle', `${javascript}\nreturn { fit: fitLine1, restore: typeof restoreLine1Fields === 'function' ? restoreLine1Fields : undefined };`)(() => ({ columnGap: '2px' }))

class Style {
  values = new Map()
  get display() { return this.getPropertyValue('display') }
  getPropertyValue(name) { return this.values.get(name) ?? '' }
  setProperty(name, value) { this.values.set(name, value) }
  removeProperty(name) { this.values.delete(name) }
}
function fixture(reversed, withCollision = false) {
  const scope = { style: new Style(), querySelectorAll: () => rows }
  const track = () => Number.parseFloat(scope.style.getPropertyValue('--sidebar-metric-track')) || 140
  const field = (kind, width) => ({
    dataset: { line1Drop: kind }, style: new Style(), width,
    getBoundingClientRect() { return { width: this.style.display === 'none' ? 0 : width, top: 0, bottom: 20, right: 256 - track() - 2 } },
  })
  const makeRow = (base, fields, titleBudget = 202) => {
    const width = () => base + fields.reduce((sum, item) => sum + (item.style.display === 'none' || item.dataset.line1Drop === 'subs' ? 0 : item.width), 0)
    const content = { get scrollWidth() { return width() }, getBoundingClientRect: () => ({ width: width(), left: 256 - width() }) }
    const title = {
      get clientWidth() { return titleBudget - track() - fields.reduce((sum, item) => sum + (item.dataset.line1Drop === 'subs' && item.style.display !== 'none' ? item.width : 0), 0) },
      getBoundingClientRect: () => ({ top: 0, bottom: 20 }),
    }
    const time = { firstElementChild: content, get clientWidth() { return track() } }
    return { clientWidth: 256, dataset: {}, querySelector: selector => {
      if (selector === '[data-row-column="title-text"]') return title
      if (selector === '[data-row-column="time"]') return time
      return fields.find(item => selector.includes(`[data-line1-drop="${item.dataset.line1Drop}"]`)) ?? null
    } }
  }
  // Roster row 91: a 21px duration plus a 25px scope. A 46px track leaves a 156px title, above the 153.6px floor.
  // A peer's transient 140px token+value track causes the monotone batch to drop this scope before shrinking to 32px.
  const durationScope = field('scope', 25), oversizeTokens = field('tokens', 108)
  const subagentCount = field('subs', 16)
  const candidate = makeRow(21, withCollision ? [durationScope, subagentCount] : [durationScope], withCollision ? 218 : 202), outlier = makeRow(32, [oversizeTokens])
  // A persistent 16px chevron gives this irreducible row less title room; it has no optional field to drop.
  const irreducible = makeRow(32, [], 186)
  const rows = reversed ? [irreducible, candidate, outlier] : [outlier, candidate, irreducible]
  fit(new Set([scope]))
  assert.equal(durationScope.style.display, '', 'row 91 scope must be restored once the cohort track shrinks')
  assert.equal(oversizeTokens.style.display, 'none', '140px tokens cannot fit the title floor')
  assert.equal(scope.style.getPropertyValue('--sidebar-metric-track'), '46px')
  assert.equal(candidate.dataset.rowTimeNatural, '46')
  assert.ok(202 - track() >= 256 * 0.6)
  if (withCollision) assert.equal(subagentCount.style.display, '', 'a count that fits the final expanded track must be retained')
  const before = rows.map(row => row.dataset.rowTimeNatural)
  fit(new Set([scope]))
  assert.deepEqual(rows.map(row => row.dataset.rowTimeNatural), before, 'settled fitting must be a fixed point')
  return { track: track(), scope: durationScope.style.display, tokens: oversizeTokens.style.display }
}
assert.deepEqual(fixture(false), fixture(true), 'row mount order must not change retention')
assert.deepEqual(fixture(false, true), fixture(true, true), 'collision must be evaluated at the final expanded track')
// A rejected widest candidate cannot support another candidate's collision clearance. These two trials must both roll back.
for (const reversed of [false, true]) {
  const makeTrial = (width, overlap) => {
    const field = { dataset: { line1Drop: 'scope' }, style: new Style() }
    field.style.setProperty('display', 'none')
    const natural = () => field.style.display === 'none' ? 32 : width
    return {
      row: { dataset: {} }, title: { clientWidth: 191.6 }, time: { clientWidth: 32 },
      content: { get scrollWidth() { return natural() }, getBoundingClientRect: () => ({ width: natural(), left: 256 - natural() }) },
      count: { getBoundingClientRect: () => ({ width: 16, right: 256 - width + overlap - 1.5 }) },
      drops: [field], floor: 153.6, gap: 2, naturalTime: 32,
    }
  }
  const a = makeTrial(50, 25), b = makeTrial(60, 40), scope = { style: new Style() }
  const cohort = { scope, rows: reversed ? [b, a] : [a, b], widest: 32, widestSignals: 0 }
  restore([cohort])
  assert.equal(a.drops[0].style.display, 'none', '50px alone cannot clear the 25px overlap')
  assert.equal(b.drops[0].style.display, 'none', '60px cannot clear the 40px overlap')
  assert.equal(scope.style.getPropertyValue('--sidebar-metric-track'), '32px', 'only accepted widths may set the final track')
}
console.log(JSON.stringify({ pass: true, fixture: 'roster row 91 with oversized peer and irreducible chevron', permutations: 6, fixedPoint: true, finalTrackCollision: true, rejectedWidestCollision: true }))
