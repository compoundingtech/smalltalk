// One palette, named by meaning, shared with stui through fixtures/clients/theme.json.
// Screens ask for `colors.working`, never for a raw colour. Mauve (`person`) means a person is
// needed and is used for nothing else.

const base = {
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

export const colors = {
  ...base,
  /** Focus, the selected tab, links. */
  accent: base.blue,
  /** A person is needed. Reserved. */
  person: base.mauve,
  working: base.peach,
  idle: base.green,
  done: base.teal,
  fault: base.red,
  waiting: base.yellow,
  quiet: base.overlay0,
  row_selected: base.surface0,
  /** The person's own messages in a conversation. */
  user_bg: '#28293d',
  tool_bg: '#232436',
  tool_ok_bg: '#1f2b2a',
  tool_err_bg: '#33222c',
  selection_bg: '#45476a',
} as const;

export type ColorToken = keyof typeof colors;

export const theme = {
  name: 'Catppuccin Mocha',
  rule: 'person (mauve) means a person is needed and is used for nothing else',
  colors,
} as const;
