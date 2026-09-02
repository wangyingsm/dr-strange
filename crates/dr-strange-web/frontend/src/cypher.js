// The query language's editing aids, shared by the Query page and Explore's
// plot box. One copy, because two would drift the moment the language grows a
// keyword — and the completion list is the only in-app documentation of what
// the box accepts.

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
 * True when the text ends inside an unterminated string literal. The words
 * typed there are data — a document's text, an entity's key — not syntax, so
 * completing them to keywords is noise. The language has no escapes, so this
 * scan is exact.
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
 * The greyed completion to show after the caret: the rest of the keyword the
 * word being typed is a prefix of, or '' when nothing is being typed, the word
 * is too short to guess from, or the caret is inside a string.
 *
 * Case-insensitive matching, but the keyword's own casing is what completes,
 * so the lower-case algorithm names stay lower-case.
 */
export function ghost(text) {
  if (inStringLiteral(text)) return ''
  const m = text.match(/([A-Za-z]+)$/) // the word currently being typed
  if (!m || m[1].length < 2) return ''
  const up = m[1].toUpperCase()
  const kw = KEYWORDS.find((k) => k.toUpperCase().startsWith(up) && k.length > up.length)
  return kw ? kw.slice(m[1].length) : ''
}

/**
 * A result cell as text: JSON null is an absent value, and a list or map shows
 * as the JSON it is rather than `[object Object]`.
 */
export function cell(v) {
  if (v === null || v === undefined) return '—'
  return typeof v === 'object' ? JSON.stringify(v) : String(v)
}

/**
 * A projected table as tab-separated text, header included — what a clipboard
 * hands to a spreadsheet or a terminal. Tabs rather than commas: a cell may
 * hold a JSON list full of commas, and nothing here should need quoting rules.
 */
export function toTsv({ columns, rows }) {
  const line = (cells) => cells.map((c) => cell(c).replace(/[\t\n]+/g, ' ')).join('\t')
  return [line(columns), ...rows.map(line)].join('\n')
}
