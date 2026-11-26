// Pure layout for file and folder names: grapheme-safe wrapping, smart ellipsis, and shared-lead dimming.
//
// Core rule: two different names in a listing never look the same.

const ELLIPSIS = '…';
const SHARED_CUT_MIN = 8;
const SHARED_KEEP_PREFIX = 6;
const SHARED_DIM_MIN = 10;
const ENDING_STEM_KEEP = 8;

const RE_BREAK_AFTER = /^[\s_\-.]$/;
const RE_LETTER = /\p{L}/u;
const RE_DIGIT = /\p{Nd}/u;
const RE_LOWER = /\p{Ll}/u;
const RE_UPPER = /\p{Lu}/u;

const GRAPHEME_SEGMENTER =
  typeof Intl !== 'undefined' && typeof Intl.Segmenter === 'function'
    ? new Intl.Segmenter(undefined, { granularity: 'grapheme' })
    : null;

/**
 * Splits a string into an array of grapheme clusters so emoji, combining marks, and RTL sequences
 * are never torn apart.
 */
export function toGraphemes(text) {
  const s = String(text ?? '');
  if (!s) return [];
  if (GRAPHEME_SEGMENTER) {
    const out = [];
    for (const seg of GRAPHEME_SEGMENTER.segment(s)) {
      out.push(seg.segment);
    }
    return out;
  }
  return Array.from(s);
}

/**
 * Returns `true` when a natural line break or dimming boundary sits between `graphemes[index - 1]`
 * and `graphemes[index]`.
 *
 * Natural breaks occur after whitespace, `_`, `-`, `.`, between a letter and a digit (either
 * direction), and between a lower-case and an upper-case letter.
 */
export function isNaturalBreak(graphemes, index) {
  if (index <= 0 || index >= graphemes.length) return false;
  const prev = graphemes[index - 1];
  const curr = graphemes[index];
  if (RE_BREAK_AFTER.test(prev)) return true;
  if ((RE_LETTER.test(prev) && RE_DIGIT.test(curr)) || (RE_DIGIT.test(prev) && RE_LETTER.test(curr))) {
    return true;
  }
  if (RE_LOWER.test(prev) && RE_UPPER.test(curr)) return true;
  return false;
}

/**
 * Finds the extension (if any) and the start index of the preserved ending (the extension plus up
 * to 8 graphemes before it, or the last 8 graphemes when there is no extension).
 *
 * Dotfiles without a second dot (`.gitignore`, `.env`) and trailing dots (`name.`) have no
 * extension.
 */
export function splitNameEnding(graphemes) {
  const n = graphemes.length;
  let dotIndex = -1;
  for (let i = n - 1; i > 0; i -= 1) {
    if (graphemes[i] === '.') {
      if (i < n - 1) dotIndex = i;
      break;
    }
  }
  if (dotIndex > 0) {
    return {
      dotIndex,
      stemEnd: dotIndex,
      extLen: n - dotIndex,
      endingStart: Math.max(0, dotIndex - ENDING_STEM_KEEP),
    };
  }
  return {
    dotIndex: -1,
    stemEnd: n,
    extLen: 0,
    endingStart: Math.max(0, n - ENDING_STEM_KEEP),
  };
}

function commonPrefixLen(a, b) {
  const lim = Math.min(a.length, b.length);
  let i = 0;
  while (i < lim && a[i] === b[i]) i += 1;
  return i;
}

function commonSuffixLen(a, b) {
  const lim = Math.min(a.length, b.length);
  let i = 0;
  while (i < lim && a[a.length - 1 - i] === b[b.length - 1 - i]) i += 1;
  return i;
}

function joinSlice(tokens, start, end) {
  let out = '';
  for (let i = start; i < end; i += 1) out += tokens[i].g;
  return out;
}

/**
 * Wraps a token list into at most `maxLines` lines, each measuring `<= maxWidth`, preferring
 * natural break points and breaking inside a word only when a single word exceeds `maxWidth`.
 *
 * Returns `null` if the tokens do not fit in `maxLines` lines.
 */
function wrapTokens(tokens, measure, maxWidth, maxLines) {
  const total = tokens.length;
  if (total === 0) return [{ start: 0, end: 0, text: '' }];

  const lineRanges = [];
  let pos = 0;

  while (pos < total) {
    if (lineRanges.length >= maxLines) return null;

    const restText = joinSlice(tokens, pos, total);
    if (measure(restText) <= maxWidth) {
      lineRanges.push({ start: pos, end: total, text: restText });
      pos = total;
      break;
    }

    // Binary search the longest prefix from `pos` that fits in `maxWidth`.
    let lo = pos + 1;
    let hi = total;
    let best = pos;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (measure(joinSlice(tokens, pos, mid)) <= maxWidth) {
        best = mid;
        lo = mid + 1;
      } else {
        hi = mid - 1;
      }
    }

    // If a single grapheme exceeds `maxWidth` (extremely tiny box), fail unless on an impossible
    // fallback caller; never produce an infinite loop or a line wider than `maxWidth`.
    if (best === pos) {
      return null;
    }

    // Account for tiny non-monotonic kerning adjustments if needed.
    while (best > pos && measure(joinSlice(tokens, pos, best)) > maxWidth) {
      best -= 1;
    }
    if (best === pos) return null;

    // Prefer the last natural break within `(pos, best]`.
    let breakAt = -1;
    for (let i = best; i > pos; i -= 1) {
      if (tokens[i].breakBefore) {
        breakAt = i;
        break;
      }
    }

    const end = breakAt > pos ? breakAt : best;
    const text = joinSlice(tokens, pos, end);
    // Guard against kerning making the shorter slice wider than `maxWidth`.
    if (measure(text) > maxWidth) {
      let shrunk = end - 1;
      while (shrunk > pos && measure(joinSlice(tokens, pos, shrunk)) > maxWidth) {
        shrunk -= 1;
      }
      if (shrunk === pos) return null;
      lineRanges.push({ start: pos, end: shrunk, text: joinSlice(tokens, pos, shrunk) });
      pos = shrunk;
    } else {
      lineRanges.push({ start: pos, end, text });
      pos = end;
    }
  }

  return lineRanges;
}

function buildResultFromWrap(tokens, lineRanges, truncated, dimChars) {
  const lines = [];
  const lineTexts = [];
  for (const range of lineRanges) {
    const spans = [];
    for (let i = range.start; i < range.end; i += 1) {
      const tok = tokens[i];
      if (spans.length > 0 && spans[spans.length - 1].dim === tok.dim) {
        spans[spans.length - 1].text += tok.g;
      } else {
        spans.push({ text: tok.g, dim: tok.dim });
      }
    }
    if (spans.length === 0) spans.push({ text: '', dim: false });
    lines.push(spans);
    lineTexts.push(range.text);
  }
  return {
    lines,
    lineTexts,
    text: lineTexts.join(''),
    truncated,
    dimChars,
  };
}

/**
 * Builds the token array for a sequence of kept `[start, end)` original grapheme ranges separated
 * by `…` (never adding an ellipsis before the first range unless the first range is empty `[0, 0]`,
 * and never ending with an ellipsis).
 */
function buildRangeTokens(G, naturalBreaks, ranges, dimEnd) {
  const tokens = [];
  for (let r = 0; r < ranges.length; r += 1) {
    const [start, end] = ranges[r];
    if (r > 0) {
      const prevEnd = ranges[r - 1][1];
      // The ellipsis steps back only when the entire elided stretch belonged to the dimmed lead.
      const ellDim = prevEnd < dimEnd && start <= dimEnd;
      tokens.push({ g: ELLIPSIS, dim: ellDim, breakBefore: false });
    }
    for (let i = start; i < end; i += 1) {
      const atRangeStartAfterEllipsis = r > 0 && i === start;
      tokens.push({
        g: G[i],
        dim: i < dimEnd,
        breakBefore: atRangeStartAfterEllipsis || (tokens.length > 0 && naturalBreaks[i]),
      });
    }
  }
  return tokens;
}

function candidateText(G, ranges) {
  let out = '';
  for (let r = 0; r < ranges.length; r += 1) {
    if (r > 0) out += ELLIPSIS;
    const [start, end] = ranges[r];
    for (let i = start; i < end; i += 1) out += G[i];
  }
  return out;
}

/**
 * Decides how a single file or folder name is wrapped, shortened, and dimmed for a listing row,
 * grid cell, pinned tile, transfer item, or viewer header.
 *
 * @param {string} name
 * @param {string | null | undefined} before
 * @param {string | null | undefined} after
 * @param {(text: string) => number} measure
 * @param {number} width
 * @param {number} [lines=2]
 * @param {{ dim?: boolean, peers?: string[] }} [opts]
 */
export function fitName(name, before, after, measure, width, lines = 2, opts = {}) {
  const raw = String(name ?? '');
  const G = toGraphemes(raw);
  const N = G.length;
  if (N === 0) {
    return {
      lines: [[{ text: '', dim: false }]],
      lineTexts: [''],
      text: '',
      truncated: false,
      dimChars: 0,
    };
  }

  const maxLines = Math.max(1, Math.floor(Number(lines) || 1));
  const maxWidth = Math.max(1, Number(width) || 1);
  const allowDim = opts?.dim !== false;

  const naturalBreaks = new Array(N).fill(false);
  for (let i = 1; i < N; i += 1) {
    naturalBreaks[i] = isNaturalBreak(G, i);
  }

  const B = typeof before === 'string' && before.length > 0 && before !== raw ? toGraphemes(before) : null;
  const A = typeof after === 'string' && after.length > 0 && after !== raw ? toGraphemes(after) : null;
  const lcpBefore = B ? commonPrefixLen(G, B) : 0;
  const lcpAfter = A ? commonPrefixLen(G, A) : 0;
  const sharedLen = Math.max(lcpBefore, lcpAfter);

  // Rule 3: dim shared beginning (>= 10 graphemes), snapped back to the last natural break before
  // the first difference.
  let dimEnd = 0;
  if (allowDim && sharedLen >= SHARED_DIM_MIN) {
    const limit = Math.min(sharedLen, N - 1);
    for (let i = limit; i >= 1; i -= 1) {
      if (naturalBreaks[i]) {
        dimEnd = i;
        break;
      }
    }
  }

  // Try the full name without any ellipsis first.
  const fullTokens = buildRangeTokens(G, naturalBreaks, [[0, N]], dimEnd);
  const fullWrap = wrapTokens(fullTokens, measure, maxWidth, maxLines);
  if (fullWrap) {
    return buildResultFromWrap(fullTokens, fullWrap, false, dimEnd);
  }

  // Collect all distinct peers we must stay distinguishable from (neighbours + any explicit peers).
  const peerList = [];
  const seenPeers = new Set([raw]);
  const addPeer = (str, preG = null) => {
    if (typeof str !== 'string' || !str || seenPeers.has(str)) return;
    seenPeers.add(str);
    const pg = preG || toGraphemes(str);
    const lcp = commonPrefixLen(G, pg);
    const lcs = commonSuffixLen(G, pg);
    peerList.push({ g: pg, text: str, lcp, lcs });
  };
  if (B) addPeer(before, B);
  if (A) addPeer(after, A);
  if (Array.isArray(opts?.peers)) {
    for (const p of opts.peers) addPeer(p);
  }

  const { stemEnd, endingStart } = splitNameEnding(G);

  const tryRanges = (ranges) => {
    const tokens = buildRangeTokens(G, naturalBreaks, ranges, dimEnd);
    const wrapped = wrapTokens(tokens, measure, maxWidth, maxLines);
    if (!wrapped) return null;
    return { tokens, wrapped, ranges };
  };

  // Determine the earliest ending start that can fit alongside a leading ellipsis.
  let effEndingStart = endingStart;
  if (!tryRanges([[0, 0], [effEndingStart, N]])) {
    // Very long extension or tiny width: keep as much of the tail as fits after `…`.
    let lo = effEndingStart + 1;
    let hi = N - 1;
    let found = N - 1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (tryRanges([[0, 0], [mid, N]])) {
        found = mid;
        hi = mid - 1;
      } else {
        lo = mid + 1;
      }
    }
    effEndingStart = found;
  }

  const distinguishesOneEllipsis = (L, R) => {
    const text = candidateText(G, [[0, L], [R, N]]);
    for (const p of peerList) {
      if (text === p.text) return false;
      // When G and P share both [0..L) and the trailing (N-R) graphemes, they could collapse.
      if (L <= p.lcp && N - R <= p.lcs) {
        // Only treat as distinguishable if G has no character outside prefix+suffix overlap
        // (checked later by fitNameList if needed).
        return false;
      }
    }
    return true;
  };

  // Given a fixed L, find the smallest R in [minR, maxR] that fits.
  const minFittingR = (L, minR, maxR) => {
    if (minR > maxR) return null;
    if (!tryRanges([[0, L], [maxR, N]])) return null;
    let lo = minR;
    let hi = maxR;
    let best = maxR;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (tryRanges([[0, L], [mid, N]])) {
        best = mid;
        hi = mid - 1;
      } else {
        lo = mid + 1;
      }
    }
    return best;
  };

  // Given a fixed R, find the largest L in [minL, maxL] that fits.
  const maxFittingL = (R, minL, maxL) => {
    if (minL > maxL) return null;
    if (!tryRanges([[0, minL], [R, N]])) return null;
    let lo = minL;
    let hi = maxL;
    let best = minL;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (tryRanges([[0, mid], [R, N]])) {
        best = mid;
        lo = mid + 1;
      } else {
        hi = mid - 1;
      }
    }
    return best;
  };

  const accept = (fit) => buildResultFromWrap(fit.tokens, fit.wrapped, true, dimEnd);

  // ── Strategy A: shared leading stretch >= 8 -> keep first 6 graphemes and cut from there ───
  if (sharedLen >= SHARED_CUT_MIN) {
    const startL = Math.min(SHARED_KEEP_PREFIX, sharedLen, effEndingStart - 1);

    // 1. Try keeping first 6 chars (`L = startL`), cutting as little as possible from index 6.
    // When the right boundary `r` falls mid-word inside the redundant shared stretch, snap it
    // forward to the next natural break inside that stretch so no stray letter fragment follows `…`.
    if (startL >= 1) {
      const r = minFittingR(startL, startL + 1, effEndingStart);
      if (r !== null && distinguishesOneEllipsis(startL, r)) {
        let chosenR = r;
        const snapLimit = Math.min(sharedLen, effEndingStart);
        if (chosenR < snapLimit && !naturalBreaks[chosenR]) {
          for (let cand = chosenR + 1; cand <= snapLimit; cand += 1) {
            if (naturalBreaks[cand]) {
              if (distinguishesOneEllipsis(startL, cand) && tryRanges([[0, startL], [cand, N]])) {
                chosenR = cand;
              }
              break;
            }
          }
        }
        const fit = tryRanges([[0, startL], [chosenR, N]]);
        if (fit) return accept(fit);
      }
    }

    // 2. If `L = startL` fit `effEndingStart` OR overflowed by a couple chars, first check if
    // placing the single ellipsis AFTER the differing prefix (while still preserving the ending)
    // distinguishes every peer — e.g. `Prefix_Q1…Approved.pdf`.
    if (peerList.length > 0 && effEndingStart > 1) {
      let needL = 0;
      for (const p of peerList) {
        if (N - effEndingStart <= p.lcs) {
          needL = Math.max(needL, p.lcp + 1);
        }
      }
      if (needL > 0 && needL < effEndingStart) {
        const bestL = maxFittingL(effEndingStart, needL, effEndingStart - 1);
        if (bestL !== null) {
          const r = minFittingR(bestL, bestL + 1, effEndingStart);
          if (r !== null && distinguishesOneEllipsis(bestL, r)) {
            const fit = tryRanges([[0, bestL], [r, N]]);
            if (fit) return accept(fit);
          }
        }
      }
    }

    // 3. Try reducing `L` from `startL - 1` down to `0` so the right half reaches the difference
    // while keeping a single ellipsis.
    for (let L = Math.max(0, startL - 1); L >= 0; L -= 1) {
      const r = minFittingR(L, L + 1, effEndingStart);
      if (r !== null && distinguishesOneEllipsis(L, r)) {
        const fit = tryRanges([[0, L], [r, N]]);
        if (fit) return accept(fit);
      }
    }
  } else {
    // ── Strategy B: no shared stretch >= 8 -> ellipsis in the middle of the stem ─────────────
    // Grow stem characters evenly on both sides of the stem midpoint while preserving the ending.
    const rightBaseStem = Math.max(0, stemEnd - effEndingStart);
    const maxStemKeep = Math.max(0, stemEnd - 1);

    const balancedPair = (stemKeep) => {
      let L = Math.ceil(stemKeep / 2);
      let rightStem = stemKeep - L;
      if (rightStem < rightBaseStem) {
        rightStem = rightBaseStem;
        L = Math.max(0, stemKeep - rightStem);
      }
      let R = stemEnd - rightStem;
      if (R > effEndingStart) R = effEndingStart;
      if (L >= R) L = Math.max(0, R - 1);
      return [L, R];
    };

    let lo = rightBaseStem;
    let hi = maxStemKeep;
    let bestPair = null;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      const [L, R] = balancedPair(mid);
      if (L < R && tryRanges([[0, L], [R, N]])) {
        bestPair = [L, R];
        lo = mid + 1;
      } else {
        hi = mid - 1;
      }
    }

    if (bestPair) {
      let [L, R] = bestPair;
      // Use any spare pixels by extending left then pulling right slightly inward.
      while (L + 1 < R && tryRanges([[0, L + 1], [R, N]])) L += 1;
      while (R - 1 > L && tryRanges([[0, L], [R - 1, N]])) R -= 1;

      if (distinguishesOneEllipsis(L, R)) {
        const fit = tryRanges([[0, L], [R, N]]);
        if (fit) return accept(fit);
      }
    }

    // If a peer still collided with the balanced middle cut, shift the cut so it includes the
    // differing prefix or suffix with a single ellipsis.
    if (peerList.length > 0 && effEndingStart > 1) {
      let needL = 0;
      for (const p of peerList) {
        if (N - effEndingStart <= p.lcs) {
          needL = Math.max(needL, p.lcp + 1);
        }
      }
      if (needL > 0 && needL < effEndingStart) {
        const bestL = maxFittingL(effEndingStart, needL, effEndingStart - 1);
        if (bestL !== null) {
          const r = minFittingR(bestL, bestL + 1, effEndingStart);
          if (r !== null && distinguishesOneEllipsis(bestL, r)) {
            const fit = tryRanges([[0, bestL], [r, N]]);
            if (fit) return accept(fit);
          }
        }
      }
      for (let L = Math.min(effEndingStart - 1, 6); L >= 0; L -= 1) {
        const r = minFittingR(L, L + 1, effEndingStart);
        if (r !== null && distinguishesOneEllipsis(L, r)) {
          const fit = tryRanges([[0, L], [r, N]]);
          if (fit) return accept(fit);
        }
      }
    }
  }

  // ── Strategy C: windowed difference + ending (only when BOTH shared lead and shared tail ───
  // individually exceed the width budget so no 1-ellipsis cut can show both diff and ending).
  if (peerList.length > 0) {
    const diffIndices = [];
    for (const p of peerList) {
      if (p.lcp < effEndingStart && p.lcp < N) {
        diffIndices.push(p.lcp);
      } else {
        const fromRight = N - p.lcs - 1;
        if (fromRight >= 0 && fromRight < effEndingStart) {
          diffIndices.push(fromRight);
        }
      }
    }
    if (diffIndices.length > 0) {
      // Pick the best initial window that fits WITH the full ending (`effEndingStart`) intact:
      // 1. Full span of all diff indices if it fits alongside `effEndingStart`.
      // 2. Otherwise the deepest diff index (closest peer) as a 1-grapheme anchor so the ending
      //    is never trimmed when a tighter diff window fits.
      const minD = Math.min(...diffIndices);
      const maxD = Math.max(...diffIndices);
      let w0 = minD;
      let w1 = maxD + 1;
      let R = effEndingStart;

      if (!tryRanges([[0, 0], [w0, w1], [R, N]])) {
        // Choose the single diff index that distinguishes the most peers (tie-breaking on deepest
        // `d`, which separates the closest sibling in sorted listings).
        const uniqueDiffs = Array.from(new Set(diffIndices)).sort((a, b) => b - a);
        let bestAnchor = uniqueDiffs[0];
        let bestScore = -1;
        for (const d of uniqueDiffs) {
          let score = 0;
          for (const p of peerList) {
            if (d >= p.g.length || G[d] !== p.g[d]) score += 1;
          }
          if (score > bestScore) {
            bestScore = score;
            bestAnchor = d;
          }
        }
        w0 = bestAnchor;
        w1 = bestAnchor + 1;
      }

      // Only relax `R` past `effEndingStart` if even a 1-grapheme diff window cannot fit beside
      // the full ending (e.g. a very long extension on a very narrow 1-line box).
      if (!tryRanges([[0, 0], [w0, w1], [R, N]])) {
        for (let candR = effEndingStart + 1; candR <= N - 1; candR += 1) {
          if (candR <= w1) continue;
          if (tryRanges([[0, 0], [w0, w1], [candR, N]])) {
            R = candR;
            break;
          }
        }
      }

      if (w1 < R && tryRanges([[0, 0], [w0, w1], [R, N]])) {
        // Expand the difference window toward any other diff indices first, then symmetrically,
        // while keeping `R` intact.
        let grew = true;
        while (grew) {
          grew = false;
          if (w0 > minD && w0 - 1 > 0 && tryRanges([[0, 0], [w0 - 1, w1], [R, N]])) {
            w0 -= 1;
            grew = true;
          }
          if (w1 <= maxD && w1 + 1 < R && tryRanges([[0, 0], [w0, w1 + 1], [R, N]])) {
            w1 += 1;
            grew = true;
          }
        }

        // Keep a short leading prefix (up to 6 graphemes) when room allows after covering diffs.
        let L = 0;
        const maxLead = Math.min(SHARED_KEEP_PREFIX, w0 - 1);
        for (let candL = maxLead; candL >= 1; candL -= 1) {
          if (tryRanges([[0, candL], [w0, w1], [R, N]])) {
            L = candL;
            break;
          }
        }

        // Expand the difference window symmetrically into any remaining space.
        grew = true;
        while (grew) {
          grew = false;
          if (w0 - 1 > L && tryRanges([[0, L], [w0 - 1, w1], [R, N]])) {
            w0 -= 1;
            grew = true;
          }
          if (w1 + 1 < R && tryRanges([[0, L], [w0, w1 + 1], [R, N]])) {
            w1 += 1;
            grew = true;
          }
        }

        // Collapse adjacent ranges if expansion touched L or R.
        const ranges = [];
        if (L > 0 && w0 <= L) {
          ranges.push([0, w1]);
        } else {
          ranges.push([0, L], [w0, w1]);
        }
        if (w1 >= R) {
          ranges[ranges.length - 1] = [ranges[ranges.length - 1][0], N];
        } else {
          ranges.push([R, N]);
        }
        const fit = tryRanges(ranges);
        if (fit) return accept(fit);
      }
    }
  }

  // ── Strategy D: best-effort 1-ellipsis fallback preserving the ending ──────────────────────
  const fallbackL = maxFittingL(effEndingStart, 0, Math.max(0, effEndingStart - 1));
  if (fallbackL !== null) {
    const r = minFittingR(fallbackL, fallbackL + 1, effEndingStart);
    if (r !== null) {
      const fit = tryRanges([[0, fallbackL], [r, N]]);
      if (fit) return accept(fit);
    }
  }

  // Absolute last resort (single grapheme wider than maxWidth): return trailing grapheme.
  const lastG = G[N - 1];
  return {
    lines: [[{ text: lastG, dim: false }]],
    lineTexts: [lastG],
    text: lastG,
    truncated: true,
    dimChars: 0,
  };
}

/**
 * Fits an ordered listing of names, running `fitName` with each entry's displayed neighbours and
 * resolving any rare non-adjacent collisions so two different names in the listing never draw the
 * same visible text.
 *
 * @param {string[]} names
 * @param {(text: string) => number} measure
 * @param {number} width
 * @param {number} [lines=2]
 * @param {{ dim?: boolean }} [opts]
 */
export function fitNameList(names, measure, width, lines = 2, opts = {}) {
  const list = Array.isArray(names) ? names.map((n) => String(n ?? '')) : [];
  const count = list.length;
  if (count === 0) return [];

  const results = new Array(count);
  const extraPeers = new Array(count);

  for (let i = 0; i < count; i += 1) {
    results[i] = fitName(
      list[i],
      i > 0 ? list[i - 1] : null,
      i + 1 < count ? list[i + 1] : null,
      measure,
      width,
      lines,
      opts,
    );
  }

  // Resolve any non-adjacent collisions in up to 4 refinement passes.
  for (let pass = 0; pass < 4; pass += 1) {
    const buckets = new Map();
    for (let i = 0; i < count; i += 1) {
      const drawn = results[i].text;
      let bucket = buckets.get(drawn);
      if (!bucket) {
        bucket = [];
        buckets.set(drawn, bucket);
      }
      bucket.push(i);
    }

    let changed = false;
    for (const indices of buckets.values()) {
      if (indices.length < 2) continue;
      const distinctNames = new Set(indices.map((idx) => list[idx]));
      if (distinctNames.size < 2) continue;

      for (const idx of indices) {
        if (!extraPeers[idx]) extraPeers[idx] = new Set();
        for (const otherName of distinctNames) {
          if (otherName !== list[idx]) extraPeers[idx].add(otherName);
        }
        const next = fitName(
          list[idx],
          idx > 0 ? list[idx - 1] : null,
          idx + 1 < count ? list[idx + 1] : null,
          measure,
          width,
          lines,
          { ...opts, peers: Array.from(extraPeers[idx]) },
        );
        if (next.text !== results[idx].text) {
          results[idx] = next;
          changed = true;
        }
      }
    }

    if (!changed) break;
  }

  return results;
}

/**
 * Splits fitted lines' spans so occurrences of `query` (case-insensitive, matching either the full
 * trimmed query or any whitespace-separated token) are flagged with `match: true` for search-hit
 * highlighting.
 *
 * Never mutates the line breaks or drawn text returned by `fitName`.
 */
export function applySearchHighlight(lines, query) {
  const rawQuery = String(query ?? '').trim();
  if (!rawQuery || !Array.isArray(lines) || lines.length === 0) {
    return (lines || []).map((spans) => spans.map((s) => ({ text: s.text, dim: Boolean(s.dim), match: false })));
  }

  const terms = rawQuery
    .split(/\s+/)
    .map((t) => t.toLowerCase())
    .filter(Boolean);
  if (terms.length === 0) {
    return lines.map((spans) => spans.map((s) => ({ text: s.text, dim: Boolean(s.dim), match: false })));
  }

  // Match across the joined drawn text so matches that wrap across two lines highlight cleanly.
  const lineTexts = lines.map((spans) => spans.map((s) => s.text).join(''));
  const joined = lineTexts.join('');
  const lower = joined.toLowerCase();
  const hitMask = new Uint8Array(joined.length);

  for (const term of terms) {
    let from = 0;
    while (from < lower.length) {
      const idx = lower.indexOf(term, from);
      if (idx === -1) break;
      for (let k = idx; k < idx + term.length && k < hitMask.length; k += 1) {
        hitMask[k] = 1;
      }
      from = idx + Math.max(1, term.length);
    }
  }

  let offset = 0;
  return lines.map((spans) => {
    const out = [];
    for (const span of spans) {
      const text = span.text;
      if (!text) continue;
      let runStart = 0;
      let runMatch = Boolean(hitMask[offset]);
      for (let i = 1; i < text.length; i += 1) {
        const m = Boolean(hitMask[offset + i]);
        if (m !== runMatch) {
          out.push({ text: text.slice(runStart, i), dim: Boolean(span.dim), match: runMatch });
          runStart = i;
          runMatch = m;
        }
      }
      out.push({ text: text.slice(runStart), dim: Boolean(span.dim), match: runMatch });
      offset += text.length;
    }
    return out;
  });
}

