import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import { useKeyboard } from 'react-aria'
import { Button, Input, Label, SearchField } from 'react-aria-components'

import { scale, tokens } from '../ui-compat/tokens.stylex.ts'

import { terminalMatches, terminalText } from './terminalText.ts'

const searches = new Map<string, readonly Range[]>()
const activeSearches = new Map<string, readonly Range[]>()
const publishHighlights = () => {
  if (typeof Highlight === 'undefined' || !('highlights' in CSS)) return
  CSS.highlights.set('wf-terminal-find', new Highlight(...[...searches.values()].flat()))
  CSS.highlights.set(
    'wf-terminal-find-active',
    new Highlight(...[...activeSearches.values()].flat()),
  )
}

/** Local find/copy controls never subscribe to screen frames or rerender terminal rows. */
export const TerminalViewport = ({
  children,
  ended = false,
}: {
  readonly children: React.ReactNode
  readonly ended?: boolean
}) => {
  const id = React.useId()
  const viewport = React.useRef<HTMLDivElement | null>(null)
  const input = React.useRef<HTMLInputElement | null>(null)
  const currentQuery = React.useRef('')
  const matches = React.useRef<readonly Range[]>([])
  const active = React.useRef(0)
  const following = React.useRef(true)
  const [query, setQuery] = React.useState('')
  const [result, setResult] = React.useState('')
  const [copyStatus, setCopyStatus] = React.useState('')
  const search = React.useCallback(
    (step = 0, reveal = false) => {
      const element = viewport.current
      if (element === null) return
      matches.current = terminalMatches({ viewport: element, query: currentQuery.current })
      active.current =
        matches.current.length === 0
          ? 0
          : (active.current + step + matches.current.length) % matches.current.length
      const match = matches.current[active.current]
      searches.set(id, matches.current)
      activeSearches.set(id, match === undefined ? [] : [match])
      publishHighlights()
      setResult(
        currentQuery.current === ''
          ? ''
          : match === undefined
            ? 'No matches'
            : `${active.current + 1} of ${matches.current.length}`,
      )
      if (reveal && match !== undefined) {
        const node = match.startContainer.parentElement
        if (node !== null) {
          following.current = false
          node.scrollIntoView({ block: 'center', inline: 'nearest' })
        }
      }
    },
    [id],
  )
  const attach = React.useCallback(
    (element: HTMLDivElement | null) => {
      viewport.current = element
      if (element === null) return
      const follow = () => {
        if (following.current) element.scrollTop = element.scrollHeight
      }
      const content = new MutationObserver(() => {
        follow()
        search()
      })
      content.observe(element, {
        childList: true,
        characterData: true,
        attributes: true,
        attributeFilter: ['data-wrapped'],
        subtree: true,
      })
      const geometry = new ResizeObserver(follow)
      geometry.observe(element)
      follow()
      search()
      return () => {
        content.disconnect()
        geometry.disconnect()
        viewport.current = null
        searches.delete(id)
        activeSearches.delete(id)
        publishHighlights()
      }
    },
    [id, search],
  )
  const { keyboardProps } = useKeyboard({
    onKeyDown: (event) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'f') {
        event.preventDefault()
        input.current?.focus()
        input.current?.select()
      } else event.continuePropagation()
    },
  })
  return (
    <div {...keyboardProps} {...stylex.props(styles.root)}>
      <div role="toolbar" aria-label="Terminal text controls" {...stylex.props(styles.toolbar)}>
        <SearchField
          value={query}
          onChange={(value) => {
            setQuery(value)
            currentQuery.current = value
            active.current = 0
            search(0, true)
          }}
          {...stylex.props(styles.search)}
        >
          <Label {...stylex.props(styles.label)}>Find</Label>
          <Input
            ref={input}
            placeholder="Find in loaded terminal text"
            {...stylex.props(styles.input)}
            onKeyDown={(event) => {
              if (event.key === 'Enter') {
                event.preventDefault()
                search(event.shiftKey ? -1 : 1, true)
              }
            }}
          />
        </SearchField>
        <Button
          aria-label="Previous match"
          isDisabled={query === ''}
          onPress={() => search(-1, true)}
          {...stylex.props(styles.button)}
        >
          Previous
        </Button>
        <Button
          aria-label="Next match"
          isDisabled={query === ''}
          onPress={() => search(1, true)}
          {...stylex.props(styles.button)}
        >
          Next
        </Button>
        <span role="status" {...stylex.props(styles.status)}>
          {result}
        </span>
        <Button
          onPress={async () => {
            const element = viewport.current
            if (element === null) return
            const selection = document.getSelection()
            const inside =
              selection !== null &&
              !selection.isCollapsed &&
              selection.anchorNode !== null &&
              selection.focusNode !== null &&
              element.contains(selection.anchorNode) &&
              element.contains(selection.focusNode)
            try {
              await navigator.clipboard.writeText(
                terminalText({ viewport: element, selection: inside ? selection : undefined }),
              )
              setCopyStatus(inside ? 'Selection copied' : 'Loaded terminal text copied')
            } catch {
              setCopyStatus('Copy failed. Select text and use your browser’s Copy command.')
            }
          }}
          {...stylex.props(styles.button)}
        >
          Copy
        </Button>
        <Button
          onPress={() => {
            following.current = true
            const element = viewport.current
            if (element !== null) element.scrollTop = element.scrollHeight
          }}
          {...stylex.props(styles.button)}
        >
          Latest
        </Button>
      </div>
      {copyStatus !== '' && (
        <p role="status" {...stylex.props(styles.copyStatus)}>
          {copyStatus}
        </p>
      )}
      <div
        ref={attach}
        onScroll={(event) => {
          const element = event.currentTarget
          following.current = element.scrollHeight - element.scrollTop - element.clientHeight <= 8
        }}
        onCopy={(event) => {
          const element = viewport.current
          const selection = document.getSelection()
          if (
            element === null ||
            selection === null ||
            selection.isCollapsed ||
            selection.anchorNode === null ||
            selection.focusNode === null ||
            !element.contains(selection.anchorNode) ||
            !element.contains(selection.focusNode)
          )
            return
          event.clipboardData.setData('text/plain', terminalText({ viewport: element, selection }))
          event.preventDefault()
        }}
        {...stylex.props(styles.body, ended && styles.ended)}
      >
        {children}
      </div>
    </div>
  )
}

const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', flexGrow: 1, minHeight: 0, minWidth: 0 },
  toolbar: {
    display: 'flex',
    alignItems: 'center',
    flexWrap: 'wrap',
    gap: scale.space1,
    padding: scale.space1,
    borderBottomWidth: '1px',
    borderBottomStyle: 'solid',
    borderBottomColor: tokens['--ds-gray-alpha-400'],
    fontSize: '0.75rem',
    color: tokens['--ds-gray-1000'],
  },
  search: {
    display: 'flex',
    alignItems: 'center',
    gap: scale.space1,
    flexGrow: 1,
    minWidth: '9rem',
  },
  label: { paddingInline: scale.space1 },
  input: {
    minWidth: 0,
    width: '100%',
    padding: scale.space1,
    fontSize: '0.75rem',
    color: tokens['--ds-gray-1000'],
    backgroundColor: tokens['--ds-background-100'],
    borderWidth: '1px',
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-alpha-400'],
  },
  button: {
    paddingInline: scale.space2,
    height: '1.5rem',
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
  status: { whiteSpace: 'nowrap', color: tokens['--ds-gray-900'] },
  copyStatus: {
    margin: 0,
    padding: scale.space1,
    fontSize: '0.75rem',
    color: tokens['--ds-gray-900'],
  },
  body: { overflow: 'auto', flexGrow: 1, minHeight: 0, padding: scale.space2 },
  ended: { opacity: 0.55, filter: 'grayscale(0.6)' },
})
