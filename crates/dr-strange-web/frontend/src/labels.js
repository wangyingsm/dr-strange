// What colour a label is.
//
// One palette for the whole dashboard, so a `Function` in a result table is
// the same colour as a `Function` in the plot beside it.

/** Categorical, and legible on both themes. */
export const PALETTE = [
  '#7c3aed',
  '#2563eb',
  '#059669',
  '#d97706',
  '#dc2626',
  '#0891b2',
  '#db2777',
  '#65a30d',
  '#9333ea',
  '#0d9488',
]

/**
 * A label's colour, by its name.
 *
 * Deterministic rather than assigned in first-seen order: a table shows one
 * page of a result and then another, and a label that changed colour between
 * them would be telling the reader something that is not true. (The plot
 * assigns by first-seen order within one session, which is what a legend
 * wants; a table has no legend to anchor it.)
 */
export function labelColor(label) {
  let h = 0
  for (const ch of String(label)) h = (h * 31 + ch.codePointAt(0)) >>> 0
  return PALETTE[h % PALETTE.length]
}
