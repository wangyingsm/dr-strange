import { describe, expect, test } from 'bun:test'
import {
  META_NAME,
  STORAGE_KEY,
  TOO_MANY_ATTEMPTS,
  UNAUTHORIZED,
  bearerHeaders,
  describeHttpRefusal,
  forgetToken,
  isJsonAnswer,
  rememberToken,
  resolveToken,
  shouldPrompt,
  tokenFromPage,
  wsUrl,
} from './auth.js'

/** A document holding (or not) the server's meta element. */
function docWith(content) {
  return {
    querySelector(sel) {
      if (content == null || sel !== `meta[name="${META_NAME}"]`) return null
      return { getAttribute: (n) => (n === 'content' ? content : null) }
    },
  }
}

/** A sessionStorage stand-in. */
function storage(init = {}) {
  const m = new Map(Object.entries(init))
  return {
    getItem: (k) => m.get(k) ?? null,
    setItem: (k, v) => m.set(k, v),
    removeItem: (k) => m.delete(k),
    map: m,
  }
}

describe('where the token comes from', () => {
  test('the page wins: a server-issued token is never stale', () => {
    const r = resolveToken({ doc: docWith('from-page'), storage: storage({ [STORAGE_KEY]: 'typed' }) })
    expect(r).toEqual({ token: 'from-page', source: 'page' })
  })

  test('without a page token, the tab remembers what the user typed', () => {
    const r = resolveToken({ doc: docWith(null), storage: storage({ [STORAGE_KEY]: 'typed' }) })
    expect(r).toEqual({ token: 'typed', source: 'session' })
  })

  test('a tokenless page and an empty tab start with nothing', () => {
    expect(resolveToken({ doc: docWith(null), storage: storage() })).toEqual({ token: null, source: null })
    // An empty meta (or none at all, or no document) counts as absent.
    expect(tokenFromPage(docWith(''))).toBeNull()
    expect(tokenFromPage(undefined)).toBeNull()
  })

  test('a storage that throws is an empty storage', () => {
    const broken = {
      getItem() {
        throw new Error('disabled')
      },
    }
    expect(resolveToken({ doc: docWith(null), storage: broken })).toEqual({ token: null, source: null })
  })
})

describe('remembering a typed token', () => {
  test('it is kept trimmed, and blank input forgets instead', () => {
    const s = storage()
    expect(rememberToken(s, '  s3cret \n')).toBe('s3cret')
    expect(s.getItem(STORAGE_KEY)).toBe('s3cret')
    expect(rememberToken(s, '   ')).toBeNull()
    expect(s.getItem(STORAGE_KEY)).toBeNull()
  })

  test('a refused token is dropped so the next attempt asks again', () => {
    const s = storage({ [STORAGE_KEY]: 'stale' })
    forgetToken(s)
    expect(s.getItem(STORAGE_KEY)).toBeNull()
  })
})

describe('how the token travels', () => {
  test('HTTP carries it as a bearer header, or nothing at all', () => {
    expect(bearerHeaders('t', { 'content-type': 'application/json' })).toEqual({
      'content-type': 'application/json',
      authorization: 'Bearer t',
    })
    expect(bearerHeaders(null, { a: 1 })).toEqual({ a: 1 })
  })

  test('the WebSocket carries it in the query, URL-encoded, matching the page scheme', () => {
    expect(wsUrl({ protocol: 'http:', host: '127.0.0.1:7700' }, null)).toBe('ws://127.0.0.1:7700/ws')
    expect(wsUrl({ protocol: 'https:', host: 'db.example.com' }, 'a b&c')).toBe(
      'wss://db.example.com/ws?token=a%20b%26c',
    )
  })
})

describe('when to ask the user', () => {
  test('only an unauthorized answer opens the prompt', () => {
    expect(shouldPrompt(null, UNAUTHORIZED)).toBe(true)
    expect(shouldPrompt('session', UNAUTHORIZED)).toBe(true)
    expect(shouldPrompt(null, -32601)).toBe(false)
  })

  test("a token the server itself wrote into the page is never second-guessed", () => {
    expect(shouldPrompt('page', UNAUTHORIZED)).toBe(false)
  })
})

describe('an answer that is not JSON-RPC', () => {
  test('a throttled or non-JSON answer is not parsed as JSON', () => {
    expect(isJsonAnswer(200, 'application/json')).toBe(true)
    expect(isJsonAnswer(200, 'application/json; charset=utf-8')).toBe(true)
    expect(isJsonAnswer(TOO_MANY_ATTEMPTS, 'application/json')).toBe(false)
    expect(isJsonAnswer(TOO_MANY_ATTEMPTS, 'text/plain; charset=utf-8')).toBe(false)
    expect(isJsonAnswer(502, 'text/html')).toBe(false)
    expect(isJsonAnswer(200, null)).toBe(false)
  })

  test("the server's words and the wait are what the user sees", () => {
    expect(describeHttpRefusal(429, '4', 'too many failed authentication attempts; retry in 4s')).toBe(
      'too many failed authentication attempts; retry in 4s',
    )
    expect(describeHttpRefusal(429, '4', '')).toBe('too many failed authentication attempts; retry in 4s')
    expect(describeHttpRefusal(429, null, '')).toBe('too many failed authentication attempts')
    expect(describeHttpRefusal(429, 'soon', '')).toBe('too many failed authentication attempts')
    expect(describeHttpRefusal(502, null, 'bad gateway')).toBe('bad gateway (HTTP 502)')
    expect(describeHttpRefusal(500, null, '  ')).toBe('request failed (HTTP 500)')
  })
})
