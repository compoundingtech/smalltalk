/**
 * Allocation-free fuzzy scoring for the palette (fzf v1 shape: forward scan for the first
 * subsequence match, backward scan from its end for the tightest window, then score the window).
 *
 * Haystacks are prepared once per entry set (lower-cased text + word-start bitmap), so a keystroke
 * over 5k entries is one tight loop per entry with no allocation. Match positions, needed only for
 * highlighting the rows on screen, are computed separately by `matchPositions`.
 */

/** A haystack prepared once per entry set so per-keystroke scoring allocates nothing. */
export interface Prepared {
  readonly text: string
  readonly lower: string
  /** 1 where a word starts: index 0, after a separator, or a lower→upper camelCase step. */
  readonly starts: Uint8Array
}

const isSeparator = (code: number): boolean =>
  code === 32 ||
  code === 45 ||
  code === 95 ||
  code === 46 ||
  code === 47 ||
  code === 58 ||
  code === 35 ||
  code === 64 ||
  code === 183

/** Lower-cases `text` and marks word starts for the word-start bonus. */
export const prepare = (text: string): Prepared => {
  const starts = new Uint8Array(text.length)
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i)
    if (i === 0) {
      starts[i] = 1
      continue
    }
    const prev = text.charCodeAt(i - 1)
    const upper = code >= 65 && code <= 90
    const prevLower = prev >= 97 && prev <= 122
    if (isSeparator(prev) || (upper && prevLower)) starts[i] = 1
  }
  return { text, lower: text.toLowerCase(), starts }
}

const SCORE_MATCH = 16
const BONUS_WORD_START = 10
const BONUS_FIRST_CHAR = 8
const BONUS_CONSECUTIVE = 6
const PENALTY_GAP_START = 3
const PENALTY_GAP_EXTENSION = 1
const BONUS_PREFIX = 24

/**
 * Scores one lower-cased token against a prepared haystack; 0 means no match. Higher is better.
 * Rewards word starts, consecutive runs and a whole-text prefix; penalises gaps inside the window.
 */
export const scoreToken = ({
  token,
  target,
}: {
  readonly token: string
  readonly target: Prepared
}): number => {
  const n = token.length
  const hay = target.lower
  const m = hay.length
  if (n === 0) return 1
  if (n > m) return 0
  // forward: find the end of the first subsequence match
  let ti = 0
  let end = -1
  for (let i = 0; i < m; i++) {
    if (hay.charCodeAt(i) === token.charCodeAt(ti)) {
      ti++
      if (ti === n) {
        end = i
        break
      }
    }
  }
  if (end < 0) return 0
  // backward: tightest start for that end
  ti = n - 1
  let start = end
  for (let i = end; i >= 0; i--) {
    if (hay.charCodeAt(i) === token.charCodeAt(ti)) {
      ti--
      if (ti < 0) {
        start = i
        break
      }
    }
  }
  // score the window [start, end] greedily from the left
  let score = 0
  let qi = 0
  let prevMatch = -2
  let inGap = false
  for (let i = start; i <= end && qi < n; i++) {
    if (hay.charCodeAt(i) === token.charCodeAt(qi)) {
      score += SCORE_MATCH
      if (target.starts[i] === 1)
        score += qi === 0 ? BONUS_WORD_START + BONUS_FIRST_CHAR : BONUS_WORD_START
      if (prevMatch === i - 1) score += BONUS_CONSECUTIVE
      prevMatch = i
      inGap = false
      qi++
    } else {
      score -= inGap ? PENALTY_GAP_EXTENSION : PENALTY_GAP_START
      inGap = true
    }
  }
  if (start === 0) score += BONUS_PREFIX
  // a later start costs a little so equal windows prefer earlier text
  score -= Math.min(start, 12) * 0.25
  return Math.max(score, 1)
}

/** Splits a raw query into lower-cased tokens; every token must match (order-independent). */
export const tokenize = (query: string): readonly string[] => {
  const tokens: string[] = []
  for (const part of query.toLowerCase().split(' ')) if (part !== '') tokens.push(part)
  return tokens
}

/**
 * Scores tokens against a primary haystack (title) and a secondary one (detail, keywords).
 * A token that only matches secondary text earns 40% so title hits rank first. 0 = excluded.
 */
export const scoreTokens = ({
  tokens,
  primary,
  secondary,
}: {
  readonly tokens: readonly string[]
  readonly primary: Prepared
  readonly secondary: Prepared | undefined
}): number => {
  let total = 0
  for (const token of tokens) {
    const p = scoreToken({ token, target: primary })
    const s = secondary === undefined ? 0 : scoreToken({ token, target: secondary }) * 0.4
    const best = p > s ? p : s
    if (best === 0) return 0
    total += best
  }
  return total
}

/** Title indices to highlight for `query`; computed only for rows on screen. */
export const matchPositions = ({
  query,
  text,
}: {
  readonly query: string
  readonly text: string
}): ReadonlySet<number> => {
  const tokens = tokenize(query)
  const target = prepare(text)
  const hits = new Set<number>()
  for (const token of tokens) {
    if (scoreToken({ token, target }) === 0) continue
    // re-run forward/backward to recover the window, then mark greedily from its start
    let ti = 0
    let end = -1
    for (let i = 0; i < target.lower.length; i++) {
      if (target.lower[i] === token[ti]) {
        ti++
        if (ti === token.length) {
          end = i
          break
        }
      }
    }
    if (end < 0) continue
    ti = token.length - 1
    let start = end
    for (let i = end; i >= 0; i--) {
      if (target.lower[i] === token[ti]) {
        ti--
        if (ti < 0) {
          start = i
          break
        }
      }
    }
    let qi = 0
    for (let i = start; i <= end && qi < token.length; i++) {
      if (target.lower[i] === token[qi]) {
        hits.add(i)
        qi++
      }
    }
  }
  return hits
}
