// Minimal JSON-RPC 2.0 client for the drsg web backend (arch/08 §1).

let nextId = 1

// The shared auth token, injected into the page by the server when DRSG_TOKEN
// is set (see crate::assets). Absent in the zero-config local case, where the
// same-origin Origin guard authorizes the UI on its own.
const TOKEN = (typeof window !== 'undefined' && window.__DRSG_TOKEN__) || null

/** Merge the bearer token into a headers object when the server issued one. */
export function authHeaders(extra = {}) {
  return TOKEN ? { ...extra, authorization: `Bearer ${TOKEN}` } : extra
}

/** Call one JSON-RPC method over HTTP POST /rpc. Throws on an RPC error. */
export async function rpc(method, params = undefined) {
  const res = await fetch('/rpc', {
    method: 'POST',
    headers: authHeaders({ 'content-type': 'application/json' }),
    body: JSON.stringify({ jsonrpc: '2.0', method, params, id: nextId++ }),
  })
  const msg = await res.json()
  if (msg.error) throw new Error(`${msg.error.message} (code ${msg.error.code})`)
  return msg.result
}

/**
 * Subscribe to live `db.stats` notifications over the WebSocket. Calls
 * `onStats(params)` for each push and `onState(open)` on connect/disconnect.
 * Returns a disposer.
 */
export function liveStats(onStats, onState) {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws'
  // The browser WebSocket API can't set request headers, so the token rides
  // the query string (the server reads `?token=` there).
  const q = TOKEN ? `?token=${encodeURIComponent(TOKEN)}` : ''
  const ws = new WebSocket(`${proto}://${location.host}/ws${q}`)
  ws.onopen = () => onState?.(true)
  ws.onclose = () => onState?.(false)
  ws.onmessage = (e) => {
    const msg = JSON.parse(e.data)
    if (msg.method === 'db.stats') onStats(msg.params)
  }
  return () => ws.close()
}

/**
 * Subscribe to the live change feed for a plane (ROADMAP §5). Opens a WebSocket,
 * sends `plane.watch { plane, label }` on connect, and calls `onChange(params)`
 * for each `plane.change` notification ({ plane, seq, truncated, changes }).
 * `onState(open)` fires on connect/disconnect. Returns a disposer.
 */
export function liveChanges(plane, label, onChange, onState) {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws'
  const q = TOKEN ? `?token=${encodeURIComponent(TOKEN)}` : ''
  const ws = new WebSocket(`${proto}://${location.host}/ws${q}`)
  ws.onopen = () => {
    onState?.(true)
    const params = { plane }
    if (label) params.label = label
    ws.send(JSON.stringify({ jsonrpc: '2.0', method: 'plane.watch', params, id: 1 }))
  }
  ws.onclose = () => onState?.(false)
  ws.onmessage = (e) => {
    const msg = JSON.parse(e.data)
    if (msg.method === 'plane.change') onChange(msg.params)
  }
  return () => ws.close()
}
