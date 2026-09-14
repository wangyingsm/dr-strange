// Where the dashboard's bearer token comes from (arch/08 §4.1). Pure: every
// function takes the document / storage it reads so the rules are testable
// without a DOM; `rpc.js` wires them to the real ones.
//
// Three sources, in order of preference:
//   1. the page — `<meta name="drsg-token">`, which the server writes only
//      when it is loopback-bound and the request came from loopback, i.e. a
//      human on this machine. Authoritative: it *is* the server's token.
//   2. this tab's sessionStorage — what the user typed at the login prompt
//      the last time a request came back unauthorized. sessionStorage, not
//      local: it dies with the tab and is not shared across origins' tabs.
//   3. nothing — either a tokenless loopback server (the same-origin check
//      authorizes us) or a server that has yet to be asked; the first
//      unauthorized answer tells them apart.

export const META_NAME = 'drsg-token'
export const STORAGE_KEY = 'drsg.token'

/** The JSON-RPC error code the server answers a missing/wrong token with. */
export const UNAUTHORIZED = -32001

/** The token the server wrote into the page, or null. */
export function tokenFromPage(doc) {
  const meta = doc?.querySelector?.(`meta[name="${META_NAME}"]`)
  const content = meta?.getAttribute?.('content') ?? meta?.content
  return content ? String(content) : null
}

/** The token this tab remembered from a login prompt, or null. */
export function tokenFromStorage(storage) {
  try {
    const v = storage?.getItem?.(STORAGE_KEY)
    return v ? String(v) : null
  } catch {
    // Storage can throw (disabled, quota, private mode); treat as empty.
    return null
  }
}

/**
 * Resolve the starting token: `{ token, source }` with `source` one of
 * 'page' | 'session' | null. The page wins over storage: a server-issued
 * value can't be stale, a remembered one can.
 */
export function resolveToken({ doc, storage } = {}) {
  const page = tokenFromPage(doc)
  if (page) return { token: page, source: 'page' }
  const session = tokenFromStorage(storage)
  if (session) return { token: session, source: 'session' }
  return { token: null, source: null }
}

/** Keep a typed token for this tab. Empty/whitespace forgets instead. */
export function rememberToken(storage, token) {
  const t = (token ?? '').trim()
  try {
    if (t) storage?.setItem?.(STORAGE_KEY, t)
    else storage?.removeItem?.(STORAGE_KEY)
  } catch {
    // Not remembering is a nuisance, not an error: the user is asked again.
  }
  return t || null
}

/** Drop a remembered token (it was refused). */
export function forgetToken(storage) {
  rememberToken(storage, null)
}

/** `Authorization: Bearer …` merged into `extra` when there is a token. */
export function bearerHeaders(token, extra = {}) {
  return token ? { ...extra, authorization: `Bearer ${token}` } : extra
}

/**
 * The `/ws` URL for `loc` (a Location-like `{ protocol, host }`). The token
 * rides the query string because the browser WebSocket API cannot set
 * request headers; the server also accepts `Authorization: Bearer` on the
 * upgrade, which non-browser clients should prefer.
 */
export function wsUrl(loc, token) {
  const proto = loc.protocol === 'https:' ? 'wss' : 'ws'
  const q = token ? `?token=${encodeURIComponent(token)}` : ''
  return `${proto}://${loc.host}/ws${q}`
}

/**
 * Whether an unauthorized answer should open the login prompt: only when the
 * token did not come from the page. A page-issued token is the server's own;
 * if that is refused, asking the user for another cannot help.
 */
export function shouldPrompt(source, code) {
  return code === UNAUTHORIZED && source !== 'page'
}
