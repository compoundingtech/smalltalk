import { writeChangelogRegistry } from './changelog-registry.mjs'

/** Vitest global setup: tests import the registry, so derive it before any worker starts. */
export default () => {
  writeChangelogRegistry()
}
