import type { TerminalScreen } from '@smalltalk/st3-client/schema'
import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import {
  Button,
  Dialog,
  DialogTrigger,
  Input,
  Label,
  Modal,
  ModalOverlay,
  NumberField,
} from 'react-aria-components'

import { scale, tokens } from '../ui-compat/tokens.stylex.ts'

import type { TerminalResizePort } from './terminal-resize-port.ts'

/** Changing dimensions is deliberate: other viewers share the same PTY. */
export const TerminalResize = ({
  screen,
  port,
  disabledReason,
}: {
  readonly screen: TerminalScreen
  readonly port?: TerminalResizePort | undefined
  readonly disabledReason?: string | undefined
}) => {
  const [columns, setColumns] = React.useState(screen.columns)
  const [rows, setRows] = React.useState(screen.rows)
  const [status, setStatus] = React.useState<{
    readonly _tag: 'Idle' | 'Sending' | 'Result'
    readonly detail: string
  }>({ _tag: 'Idle', detail: '' })
  const unavailable =
    disabledReason ??
    (port === undefined ? 'Resize is unavailable from this data source.' : undefined)
  return (
    <DialogTrigger>
      <Button aria-label="Resize terminal" {...stylex.props(styles.button)}>
        Resize
      </Button>
      <ModalOverlay isDismissable {...stylex.props(styles.overlay)}>
        <Modal {...stylex.props(styles.modal)}>
          <Dialog aria-label="Resize shared terminal" {...stylex.props(styles.dialog)}>
            {({ close }) => (
              <>
                <h2 {...stylex.props(styles.heading)}>Resize shared terminal</h2>
                <p {...stylex.props(styles.text)}>
                  This changes the PTY for every viewer. The browser window never changes it
                  automatically.
                </p>
                <p {...stylex.props(styles.text)}>
                  Observed size: {screen.columns} columns × {screen.rows} rows.
                </p>
                <NumberField
                  value={columns}
                  onChange={setColumns}
                  minValue={1}
                  step={1}
                  isDisabled={status._tag === 'Sending'}
                  {...stylex.props(styles.field)}
                >
                  <Label>Columns</Label>
                  <Input {...stylex.props(styles.input)} />
                </NumberField>
                <NumberField
                  value={rows}
                  onChange={setRows}
                  minValue={1}
                  step={1}
                  isDisabled={status._tag === 'Sending'}
                  {...stylex.props(styles.field)}
                >
                  <Label>Rows</Label>
                  <Input {...stylex.props(styles.input)} />
                </NumberField>
                {unavailable !== undefined && (
                  <p role="status" {...stylex.props(styles.text)}>
                    {unavailable}
                  </p>
                )}
                {status.detail !== '' && (
                  <p role="status" {...stylex.props(styles.text)}>
                    {status.detail}
                  </p>
                )}
                <div {...stylex.props(styles.actions)}>
                  <Button onPress={close} {...stylex.props(styles.button)}>
                    Close
                  </Button>
                  <Button
                    isDisabled={
                      unavailable !== undefined ||
                      status._tag === 'Sending' ||
                      !Number.isSafeInteger(columns) ||
                      !Number.isSafeInteger(rows) ||
                      columns < 1 ||
                      rows < 1
                    }
                    onPress={async () => {
                      if (port === undefined) return
                      setStatus({ _tag: 'Sending', detail: 'Requesting resize…' })
                      try {
                        const result = await port.resize({
                          terminalId: screen.terminal_id,
                          incarnation: screen.runtime_incarnation,
                          columns,
                          rows,
                        })
                        setStatus({
                          _tag: 'Result',
                          detail:
                            result._tag === 'Requested'
                              ? `Resize to ${result.columns} × ${result.rows} requested. The observed terminal size confirms when it takes effect.`
                              : result.detail,
                        })
                      } catch {
                        setStatus({
                          _tag: 'Result',
                          detail:
                            'Resize could not be confirmed. Check the observed size before trying again.',
                        })
                      }
                    }}
                    {...stylex.props(styles.button)}
                  >
                    {status._tag === 'Sending' ? 'Requesting…' : 'Apply size'}
                  </Button>
                </div>
              </>
            )}
          </Dialog>
        </Modal>
      </ModalOverlay>
    </DialogTrigger>
  )
}

const styles = stylex.create({
  button: {
    height: '1.5rem',
    paddingInline: scale.space2,
    fontSize: '0.75rem',
    color: tokens['--ds-gray-1000'],
    backgroundColor: {
      default: 'transparent',
      ':is([data-hovered])': tokens['--ds-gray-alpha-100'],
    },
    borderWidth: '1px',
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-alpha-400'],
    cursor: 'pointer',
    outlineStyle: { default: 'none', ':is([data-focus-visible])': 'solid' },
    outlineColor: tokens['--ds-focus-color'],
    outlineWidth: '2px',
  },
  overlay: {
    position: 'fixed',
    inset: 0,
    backgroundColor: tokens['--ds-gray-alpha-400'],
    display: 'grid',
    placeItems: 'center',
    zIndex: 100,
  },
  modal: {
    width: 'min(28rem, 90vw)',
    backgroundColor: tokens['--ds-background-100'],
    color: tokens['--ds-gray-1000'],
    borderRadius: scale.radiusDefault,
  },
  dialog: {
    padding: scale.space4,
    outline: 'none',
    display: 'flex',
    flexDirection: 'column',
    gap: scale.space2,
  },
  heading: { margin: 0, fontSize: '1rem' },
  text: { margin: 0, fontSize: '0.8125rem' },
  field: { display: 'flex', alignItems: 'center', gap: scale.space2, fontSize: '0.8125rem' },
  input: {
    width: '6rem',
    padding: scale.space1,
    color: tokens['--ds-gray-1000'],
    backgroundColor: tokens['--ds-background-100'],
    borderWidth: '1px',
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-alpha-400'],
  },
  actions: { display: 'flex', justifyContent: 'flex-end', gap: scale.space2 },
})
