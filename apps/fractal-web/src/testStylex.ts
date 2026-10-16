// StyleX compiles its API away at build time; the published runtime throws when called directly.
// App tests load kit modules uncompiled, so every suite mocks `@stylexjs/stylex` with this runtime:
// `vi.mock('@stylexjs/stylex', () => import('../testStylex.ts'))`. Vitest hoists mock factories above
// static imports, so the factory must load it with import(). Suites that inspect styles spread it and
// override only the members they observe, so new kit API use is covered in one place.
type Pseudo = `:${string}` | `[${string}]`
type Marker = Readonly<{ marker: symbol | string }>

const defaultMarkerNamespace: Marker = Object.freeze({ marker: 'default-marker' })
// Same string shape as StyleX's typed `:where-<relation>(pseudo, marker)` condition keys.
const relation = (name: string) => (pseudo?: Pseudo, marker: Marker = defaultMarkerNamespace) =>
  `:where-${name}(${pseudo ?? ''}, ${String(marker.marker)})`

export const create = <Styles>(styles: Styles): Styles => styles
export const defineVars = <Vars>(variables: Vars): Vars => variables
export const createTheme = () => ({})
export const keyframes = () => 'test-animation'
export const props = (..._styles: readonly unknown[]) => ({})
export const defaultMarker = () => defaultMarkerNamespace
export const when = {
  ancestor: relation('ancestor'),
  descendant: relation('descendant'),
  siblingBefore: relation('siblingBefore'),
  siblingAfter: relation('siblingAfter'),
  anySibling: relation('anySibling'),
}
