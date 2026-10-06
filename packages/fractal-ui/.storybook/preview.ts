import type { Preview } from '@storybook/react-vite'
import '../src/tokens.css'

const preview: Preview = {
  parameters: {
    layout: 'fullscreen',
    controls: { expanded: true },
    a11y: { test: 'error' },
  },
}
export default preview
