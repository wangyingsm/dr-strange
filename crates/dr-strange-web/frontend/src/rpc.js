// Minimal JSON-RPC 2.0 client for the drsg web backend (arch/08 §1).

import { bearerHeaders, forgetToken, rememberToken, resolveToken, shouldPrompt, wsUrl } from './auth.js'
import { chunk, pairBatch } from './batch.js'

let nextId = 1

// The shared auth token and where it came from — see auth.js. The page
// carries one only when the server is loopback-bound and we are a loopback
// peer; a LAN deployment's page carries none, and the user is asked for it
// the first time the server answers "unauthorized".
const storage = typeof sessionStorage !== 'undefined' ? sessionStorage : undefined
let { token: TOKEN, source } = resolveToken({
  doc: typeof document !== 'undefined' ? document : undefined,
  storage,
})

/** Merge the bearer token into a headers object when we hold one. */
export function authHeaders(extra = {}) {
  return bearerHeaders(TOKEN, extra)
}

// Sockets that failed to open while we held no (or a refused) token; re-dialled
// once the user has signed in. A socket never opens the prompt itself — it
// cannot tell "unauthorized" from any other failed upgrade, and a tokenless
// loopback server that is merely down must not ask anyone for a password.
const waitingForLogin = new Set()

// One prompt at a time: the dashboard fires several RPCs at load, and every
// one of them refused must wait for the same answer, not each ask.
let pendingLogin = null

/**
 * Ask the user for the server's token (the minimal login: a prompt, kept in
 * this tab's sessionStorage). Resolves to the token, or null if dismissed.
 */
function login() {
  if (pendingLogin) return pendingLogin
  pendingLogin = new Promise((resolve) => {
    // Next tick, so concurrent callers all attach to `pendingLogin` first.
    setTimeout(() => {
      forgetToken(storage)
      const typed =
        typeof window !== 'undefined' && typeof window.prompt === 'function'
          ? window.prompt(
              'This drsg server is not on loopback, so the page carries no token.\n' +
                'Paste its DRSG_TOKEN to sign in (kept for this tab only):',
            )
          : null
      TOKEN = rememberToken(storage, typed)
      source = TOKEN ? 'session' : null
      pendingLogin = null
      if (TOKEN) {
        for (const redial of waitingForLogin) redial()
        waitingForLogin.clear()
      }
      resolve(TOKEN)
    }, 0)
  })
  return pendingLogin
}

/** Call one JSON-RPC method over HTTP POST /rpc. Throws on an RPC error. */
export async function rpc(method, params = undefined) {
  // At most one retry: a refused answer with no page token means "ask the
  // user"; a second refusal after that is reported, not asked about again.
  for (let attempt = 0; ; attempt++) {
    const res = await fetch('/rpc', {
      method: 'POST',
      headers: authHeaders({ 'content-type': 'application/json' }),
      body: JSON.stringify({ jsonrpc: '2.0', method, params, id: nextId++ }),
    })
    const msg = await res.json()
    if (!msg.error) return msg.result
    if (attempt === 0 && shouldPrompt(source, msg.error.code) && (await login())) continue
    throw new Error(`${msg.error.message} (code ${msg.error.code})`)
  }
}

/**
 * Call many methods in JSON-RPC batches, at most `MAX_BATCH` a request and one
 * request in flight at a time. `calls` is `[{ method, params }]`; the answer
 * is one `{ result }` or `{ error }` per call, in the same order — a failed
 * call does not reject the rest, since the caller of a batch usually wants
 * whatever came back and a count of what did not.
 *
 * One request at a time is deliberate: a batch is already the server doing
 * many things for one round trip, and a frontier of three hundred nodes fired
 * as three hundred connections was what this replaces.
 */
export async function rpcBatch(calls) {
  const out = []
  for (const run of chunk(calls)) {
    const requests = run.map(({ method, params }) => ({ jsonrpc: '2.0', method, params, id: nextId++ }))
    out.push(...pairBatch(requests, await postBatch(requests)))
  }
  return out
}

/** POST one batch; on a refused token, ask once and retry, as `rpc` does. */
async function postBatch(requests) {
  for (let attempt = 0; ; attempt++) {
    const res = await fetch('/rpc', {
      method: 'POST',
      headers: authHeaders({ 'content-type': 'application/json' }),
      body: JSON.stringify(requests),
    })
    const msg = await res.json()
    // A whole-batch refusal (unauthorized, too large) is a single error object.
    const code = !Array.isArray(msg) && msg?.error ? msg.error.code : null
    if (code != null && attempt === 0 && shouldPrompt(source, code) && (await login())) continue
    return msg
  }
}

/**
 * Open `/ws` and run `setup(ws)` on it; if the upgrade is refused before we
 * are signed in, dial again once the user is. Returns a disposer.
 */
function openSocket(setup) {
  let ws = null
  let disposed = false
  const dial = () => {
    if (disposed) return
    // The browser WebSocket API can't set request headers, so the token rides
    // the query string (the server reads `?token=` there, and prefers an
    // `Authorization` header from clients that can send one).
    ws = new WebSocket(wsUrl(location, TOKEN))
    let opened = false
    setup(ws, {
      onOpen: () => {
        opened = true
      },
      onClose: () => {
        // Closed without ever opening while we had no page token: most
        // likely a 401 on the upgrade. Wait for a login, then try again.
        if (!opened && !disposed && source !== 'page') waitingForLogin.add(dial)
      },
    })
  }
  dial()
  return () => {
    disposed = true
    waitingForLogin.delete(dial)
    ws?.close()
  }
}

/**
 * Subscribe to live `db.stats` notifications over the WebSocket. Calls
 * `onStats(params)` for each push and `onState(open)` on connect/disconnect.
 * Returns a disposer.
 */
export function liveStats(onStats, onState) {
  return openSocket((ws, life) => {
    ws.onopen = () => {
      life.onOpen()
      onState?.(true)
    }
    ws.onclose = () => {
      life.onClose()
      onState?.(false)
    }
    ws.onmessage = (e) => {
      const msg = JSON.parse(e.data)
      if (msg.method === 'db.stats') onStats(msg.params)
    }
  })
}

/**
 * Subscribe to the live change feed for a plane (ROADMAP §5). Opens a WebSocket,
 * sends `plane.watch { plane, label }` on connect, and calls `onChange(params)`
 * for each `plane.change` notification ({ plane, seq, truncated, changes }).
 * `onState(open)` fires on connect/disconnect. Returns a disposer.
 */
export function liveChanges(plane, label, onChange, onState) {
  return openSocket((ws, life) => {
    ws.onopen = () => {
      life.onOpen()
      onState?.(true)
      const params = { plane }
      if (label) params.label = label
      ws.send(JSON.stringify({ jsonrpc: '2.0', method: 'plane.watch', params, id: 1 }))
    }
    ws.onclose = () => {
      life.onClose()
      onState?.(false)
    }
    ws.onmessage = (e) => {
      const msg = JSON.parse(e.data)
      if (msg.method === 'plane.change') onChange(msg.params)
    }
  })
}
