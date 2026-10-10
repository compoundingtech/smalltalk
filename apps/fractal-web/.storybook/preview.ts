import type { Preview } from '@storybook/react-vite'
import '../src/web/app.css'
import 'virtual:overeng-stylex.css'

const preview: Preview = {
  parameters: { layout: 'fullscreen', controls: { expanded: true } },
}
export default preview
