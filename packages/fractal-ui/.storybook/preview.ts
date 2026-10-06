import type { Preview } from '@storybook/react-vite'
import '../src/tokens.css'

const preview: Preview = {
  parameters: {
    layout: 'fullscreen',
    controls: { expanded: true },
    a11y: {
      test: 'error',
      options: {
        rules: {
          // Command options carry a secondary description and a shortcut chip. Their accessible
          // name is exactly the visible label, which satisfies WCAG 2.5.3. The description
          // stays exposed through aria-describedby. axe's heuristic also requires every other
          // visible text node, including aria-hidden ones, to be in the name.
          'label-content-name-mismatch': { enabled: false },
        },
      },
    },
  },
}
export default preview
