import fs from "node:fs";
import path from "node:path";
import { set } from "./store.js";

const CONFIG_DIR = path.join(process.cwd(), "config");

// Load a named JSON config file from the config dir and merge its keys into the store.
export function loadConfig(name) {
  const file = path.join(CONFIG_DIR, name);
  const data = JSON.parse(fs.readFileSync(file, "utf8"));
  for (const [k, v] of Object.entries(data)) set(k, v);
  return data;
}

// Merge override values over a base config object; overrides win.
export function mergeConfig(base, overrides) {
  const result = base;
  for (const k of Object.keys(overrides)) result[k] = overrides[k];
  return result;
}
