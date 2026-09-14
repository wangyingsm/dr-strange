// The shape of a JSON-RPC batch, kept apart from the transport so it can be
// tested without one (arch/08 §1).

/**
 * The server answers at most this many requests in one batch (`rpc::MAX_BATCH`
 * in crates/dr-strange-web/src/rpc.rs); a larger array is refused whole, so a
 * client that wants more sends more batches.
 */
export const MAX_BATCH = 64

/** Split `items` into runs of at most `size`, in order. */
export function chunk(items, size = MAX_BATCH) {
  const out = []
  for (let i = 0; i < items.length; i += size) out.push(items.slice(i, i + size))
  return out
}

/**
 * Pair a batch's responses with the requests that asked them, by `id`.
 *
 * JSON-RPC lets the server answer a batch in any order, and a request it
 * could not parse is answered with a null id — so the answers are indexed by
 * id rather than trusted positionally, and a request no answer names is
 * reported as an error rather than as `undefined`, which a caller would read
 * as an empty result. Returns one `{ result }` or `{ error }` per request.
 */
export function pairBatch(requests, responses) {
  const byId = new Map()
  for (const r of Array.isArray(responses) ? responses : []) {
    if (r && r.id != null) byId.set(r.id, r)
  }
  return requests.map(({ id }) => {
    const r = byId.get(id)
    if (!r) return { error: { code: -32603, message: 'no answer in batch' } }
    return r.error ? { error: r.error } : { result: r.result }
  })
}
