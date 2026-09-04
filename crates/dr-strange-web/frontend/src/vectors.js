// Embeddings, as the server sends them and as a page shows them.
//
// A vector arrives as the marker `$vector(1024 dims, omitted)` rather than a
// thousand floats: nothing here draws an embedding, and shipping one per node
// is the difference between a hundred-kilobyte answer and a hundred-megabyte
// one. The dimension is in the marker, which is enough to label a button; the
// values are one `node.get` away, on the click that asks for them.
//
// A literal array still counts everywhere, for a caller that opted out of lean
// or a plane read before this was so.

const MARKER = /^\$vector\((\d+) dims, omitted\)$/

/**
 * How many dimensions a property value has, or `null` when it is not a vector
 * at all — which is also how to ask "is this a vector".
 */
export function vectorDims(v) {
  if (Array.isArray(v)) {
    return v.length > 0 && v.every((x) => typeof x === 'number') ? v.length : null
  }
  if (typeof v === 'string') {
    const m = v.match(MARKER)
    return m ? Number(m[1]) : null
  }
  return null
}

/**
 * The floats inside a property as the core JSON dialect writes it: a described
 * property is `{ $desc, $value }`, and a vector is `{ $vector: [...] }`. Either
 * wrapping, both, or neither. `null` when there is no vector in there —
 * a marker included, which says a vector exists but not what it is.
 */
export function unwrapVector(raw) {
  const inner = raw && typeof raw === 'object' && '$value' in raw ? raw.$value : raw
  if (Array.isArray(inner?.$vector)) return inner.$vector
  return Array.isArray(inner) && inner.every((x) => typeof x === 'number') ? inner : null
}

/** Fixed-width columns of six, so a thousand floats read as a grid. */
export function formatVector(v) {
  const cols = 6
  const rows = []
  for (let i = 0; i < v.length; i += cols) {
    rows.push(
      v
        .slice(i, i + cols)
        .map((x) => x.toFixed(5).padStart(10))
        .join(' '),
    )
  }
  return rows.join('\n')
}
