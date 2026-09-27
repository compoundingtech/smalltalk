import { defaultOptions } from "./config.js";

// Format a label with optional overrides, e.g. format("ok") -> "[ ok ]".
export function format(label, options = {}) {
  const opts = Object.assign(defaultOptions, options);
  const p = " ".repeat(opts.pad);
  return `${opts.prefix}${p}${label}${p}${opts.suffix}`;
}
