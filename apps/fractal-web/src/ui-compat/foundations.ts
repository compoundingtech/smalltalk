/** Apply Fractal's original palette at the document root, including portal content. */
export type Theme = 'light' | 'dark' | 'system'
let removeSystemListener: (() => void) | undefined
export function setTheme(theme: Theme): void {
  removeSystemListener?.()
  removeSystemListener = undefined
  const root = document.documentElement
  root.dataset.direction = 'relay'
  root.dataset.density = 'compact'
  root.dataset.workshop = ''
  const apply = (dark: boolean) => {
    root.dataset.scheme = dark ? 'dark' : 'light'
    root.style.colorScheme = dark ? 'dark' : 'light'
    root.style.setProperty('--wf-boot-canvas', dark ? '#0a0a0a' : '#fcfcfc')
    root.style.setProperty('--wf-boot-sidebar', dark ? '#000000' : '#ffffff')
    root.style.setProperty('--wf-boot-line', dark ? '#242424' : '#e4e4e7')
  }
  if (theme === 'system') {
    const preference = window.matchMedia('(prefers-color-scheme: dark)')
    apply(preference.matches)
    const changed = (event: MediaQueryListEvent) => apply(event.matches)
    preference.addEventListener('change', changed)
    removeSystemListener = () => preference.removeEventListener('change', changed)
  } else apply(theme === 'dark')
}
