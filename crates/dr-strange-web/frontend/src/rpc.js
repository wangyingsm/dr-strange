// Minimal JSON-RPC 2.0 client for the drsg web backend (arch/08 §1).

let nextId = 1

/** Call one JSON-RPC method over HTTP POST /rpc. Throws on an RPC error. */
export async function rpc(method, params = undefined) {
  const res = await fetch('/rpc', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
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
  const ws = new WebSocket(`${proto}://${location.host}/ws`)
  ws.onopen = () => onState?.(true)
  ws.onclose = () => onState?.(false)
  ws.onmessage = (e) => {
    const msg = JSON.parse(e.data)
    if (msg.method === 'db.stats') onStats(msg.params)
  }
  return () => ws.close()
}
