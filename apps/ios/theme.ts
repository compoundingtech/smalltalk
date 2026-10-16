// One palette, named by meaning, the same one stui uses (crates/stui/src/ui/theme.rs): Catppuccin
// Mocha. Mauve is reserved for "a person is needed" and nothing else uses it.
export const palette = {
  base: '#1e1e2e',
  mantle: '#181825',
  crust: '#11111b',
  surface0: '#313244',
  surface1: '#45475a',
  surface2: '#585b70',
  overlay0: '#6c7086',
  overlay1: '#7f849c',
  subtext0: '#a6adc8',
  subtext1: '#bac2de',
  text: '#cdd6f4',
  lavender: '#b4befe',
  blue: '#89b4fa',
  sapphire: '#74c7ec',
  teal: '#94e2d5',
  green: '#a6e3a1',
  yellow: '#f9e2af',
  peach: '#fab387',
  red: '#f38ba8',
  mauve: '#cba6f7',
} as const;

export const theme = {
  ...palette,
  /** Focus, the selected tab, links. */
  accent: palette.blue,
  /** A person is needed. Reserved. */
  person: palette.mauve,
  working: palette.peach,
  idle: palette.green,
  done: palette.teal,
  fault: palette.red,
  waiting: palette.yellow,
  quiet: palette.overlay0,
  rowSelected: palette.surface0,
  /** The person's own messages in a conversation. */
  userBg: '#28293d',
  toolBg: '#232436',
  toolOkBg: '#1f2b2a',
  toolErrBg: '#33222c',
} as const;

// IBM Plex Mono, loaded by the app at start. The names are the keys given to `useFonts`.
export const fonts = {
  regular: 'IBMPlexMono_400Regular',
  italic: 'IBMPlexMono_400Regular_Italic',
  semibold: 'IBMPlexMono_600SemiBold',
  bold: 'IBMPlexMono_700Bold',
} as const;

/** System appearance for migrated iPhone surfaces; legacy tabs keep their palette. */
export const systemColors = {
  light: { background: '#f2f2f7', grouped: '#ffffff', text: '#000000', secondary: '#59595f', separator: '#c6c6c8', action: '#005fcc', attention: '#9c4400', fault: '#c00f20', success: '#216e35' },
  dark: { background: '#000000', grouped: '#1c1c1e', text: '#ffffff', secondary: '#b5b5bd', separator: '#48484a', action: '#64aaff', attention: '#ffb15c', fault: '#ff6961', success: '#72d88a' },
} as const;

// stui colours each harness the same way everywhere.
export function harnessColor(harness: string): string {
  switch (harness) {
    case 'claude': return palette.peach;
    case 'codex': return palette.blue;
    case 'omp': return palette.teal;
    case 'pi': return palette.lavender;
    default: return palette.overlay0;
  }
}
