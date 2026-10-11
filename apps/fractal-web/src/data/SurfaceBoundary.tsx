import * as stylex from '@stylexjs/stylex'
import * as React from 'react'

import { scale, tokens } from '../ui-compat/tokens.stylex.ts'

/** Owns a retained surface's activation outside Activity, including deferred hidden renders. */
export const SurfaceActivity = ({
  mode,
  children,
}: {
  readonly mode: 'visible' | 'hidden'
  readonly children: React.ReactNode
}) => {
  const activation = React.useMemo(() => Symbol(`surface ${mode}`), [mode])
  return (
    <ActivationContext.Provider value={activation}>
      <React.Activity mode={mode}>{children}</React.Activity>
    </ActivationContext.Provider>
  )
}

/** A renderer defect stays latched until its subject or retained activation changes. */
export const SurfaceBoundary = (props: {
  readonly label: string
  readonly children: React.ReactNode
}) => <SurfaceBoundaryImpl {...props} activation={React.useContext(ActivationContext)} />

const ActivationContext = React.createContext(Symbol('initial surface activation'))

const styles = stylex.create({
  failure: { padding: scale.space4, color: tokens['--ds-gray-1000'], fontSize: '0.8125rem' },
  detail: { overflowWrap: 'anywhere', whiteSpace: 'pre-wrap', color: tokens['--ds-gray-900'] },
})

class SurfaceBoundaryImpl extends React.Component<
  {
    readonly label: string
    readonly children: React.ReactNode
    readonly activation: symbol
  },
  { readonly error: Error | undefined; readonly activation: symbol }
> {
  override state: { readonly error: Error | undefined; readonly activation: symbol } = {
    error: undefined,
    activation: this.props.activation,
  }

  // oxlint-disable-next-line overeng/named-args -- React's required getDerivedStateFromProps ABI supplies props and state positionally.
  static getDerivedStateFromProps(
    props: { readonly activation: symbol },
    state: { readonly activation: symbol },
  ) {
    return props.activation === state.activation
      ? null
      : { activation: props.activation, error: undefined }
  }

  static getDerivedStateFromError(error: unknown) {
    return { error: error instanceof Error ? error : new Error(String(error)) }
  }

  override render() {
    if (this.state.error === undefined) return this.props.children
    return (
      <section
        role="alert"
        aria-label={`${this.props.label} rendering failure`}
        {...stylex.props(styles.failure)}
      >
        <p>{this.props.label} could not be displayed. Other surfaces remain available.</p>
        <details>
          <summary>Error details</summary>
          <p {...stylex.props(styles.detail)}>{this.state.error.message}</p>
        </details>
      </section>
    )
  }
}
