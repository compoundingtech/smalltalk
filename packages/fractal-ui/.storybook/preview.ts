import type { Preview } from '@storybook/react-vite'
import { scenarioGlobalTypes, withScenario } from '@smalltalk/st3-scenarios/storybook'
import { withScenarioTime } from '../src/assistant-ui/scenario-presentation'
import '../src/tokens.css'

const preview: Preview = {
  globalTypes: scenarioGlobalTypes,
  decorators: [withScenario, withScenarioTime],
  parameters: {
    layout: 'fullscreen',
    controls: { expanded: true },
    a11y: { test: 'error' },
  },
}
export default preview
