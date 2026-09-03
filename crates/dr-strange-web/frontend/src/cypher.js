// The query language's editing aids, for the Query page. Kept beside the view
// rather than inside it because this keyword list is the only in-app account of
// what the box accepts, and it is tested on its own.

// Ordered shortest-prefix first, so MATCH is offered before MATCHING and the
// longer one is still reachable by typing one more character.
export const KEYWORDS = [
  // sources and the clauses that follow them
  'MATCH', 'MATCHING', 'SEARCH', 'HYBRID', 'CALL', 'BEAM', 'WHERE', 'RETURN',
  'DISTINCT', 'ORDER BY', 'SKIP', 'LIMIT', 'AS OF', 'TIME',
  // expression terms — ahead of the knobs so `key(` wins over KEYWORD, which
  // is only valid inside HYBRID
  // (`hops()` is deliberately absent: it would shadow the GRAPH channel's
  // required HOPS keyword, which is typed far more often)
  'key()', 'score()', 'similarity(', 'distance(',
  // the folds a RETURN may project with (`AS` is left out: two letters, and
  // completing it would shadow the `AS OF` above)
  'count(*)', 'collect(', 'sum(', 'avg(', 'min(', 'max(',
  // retrieval knobs: the vector/keyword seeds, the hybrid channels, the beam
  'NEAR', 'METRIC', 'TOPK', 'VECTOR', 'KEYWORD', 'GRAPH', 'HOPS', 'DECAY',
  'SEEDS', 'WEIGHT', 'CANDIDATES', 'WIDTH', 'DEPTH',
  // algorithm names for CALL — lower-case, as they read in a query (the
  // compiler folds case, so an accepted completion parses either way)
  'pagerank', 'components', 'shortest_path', 'louvain',
  // writes
  'CREATE', 'MERGE', 'SET', 'DELETE', 'REMOVE', 'DETACH',
  // operators
  'AND', 'OR', 'NOT', 'ON', 'IN', 'IS', 'NULL', 'DESC',
]

/**
 * True when the text ends inside an unterminated string literal. Words typed
 * there are data, not syntax, so completing them is noise. The language has
 * no escapes, so this scan is exact.
 */
export function inStringLiteral(s) {
  let quote = null
  for (const ch of s) {
    if (quote) {
      if (ch === quote) quote = null
    } else if (ch === '"' || ch === "'") {
      quote = ch
    }
  }
  return quote !== null
}

/**
 * The greyed completion after the caret: the rest of the keyword the current
 * word prefixes. '' when the word is under two characters or inside a string.
 * Matching is case-insensitive; the keyword's own casing completes, so the
 * lower-case algorithm names stay lower-case.
 */
export function ghost(text) {
  if (inStringLiteral(text)) return ''
  const m = text.match(/([A-Za-z]+)$/) // the word currently being typed
  if (!m || m[1].length < 2) return ''
  const up = m[1].toUpperCase()
  const kw = KEYWORDS.find((k) => k.toUpperCase().startsWith(up) && k.length > up.length)
  return kw ? kw.slice(m[1].length) : ''
}

/** A result cell as text: null is absence, a list or map shows as JSON. */
export function cell(v) {
  if (v === null || v === undefined) return '—'
  return typeof v === 'object' ? JSON.stringify(v) : String(v)
}

/**
 * A projected table as tab-separated text, header included. Tabs rather than
 * commas: a cell may hold a JSON list full of commas, and this needs no
 * quoting rules.
 */
export function toTsv({ columns, rows }) {
  const line = (cells) => cells.map((c) => cell(c).replace(/[\t\n]+/g, ' ')).join('\t')
  return [line(columns), ...rows.map(line)].join('\n')
}
