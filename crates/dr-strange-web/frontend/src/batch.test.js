import { describe, expect, test } from 'bun:test'
import { MAX_BATCH, chunk, pairBatch } from './batch.js'

describe('chunk', () => {
  test('splits in order, last run short, nothing for nothing', () => {
    expect(chunk([1, 2, 3, 4, 5], 2)).toEqual([[1, 2], [3, 4], [5]])
    expect(chunk([], 2)).toEqual([])
  })

  test('the default run fits the server limit', () => {
    const items = Array.from({ length: 300 }, (_, i) => i)
    const runs = chunk(items)
    expect(runs).toHaveLength(Math.ceil(300 / MAX_BATCH))
    expect(runs.every((r) => r.length <= MAX_BATCH)).toBe(true)
    expect(runs.flat()).toEqual(items)
  })
})

describe('pairBatch', () => {
  const reqs = [{ id: 1 }, { id: 2 }, { id: 3 }]

  test('answers are matched by id, whatever order they arrive in', () => {
    const out = pairBatch(reqs, [
      { jsonrpc: '2.0', id: 3, result: 'c' },
      { jsonrpc: '2.0', id: 1, result: 'a' },
      { jsonrpc: '2.0', id: 2, error: { code: -32602, message: 'bad' } },
    ])
    expect(out).toEqual([{ result: 'a' }, { error: { code: -32602, message: 'bad' } }, { result: 'c' }])
  })

  test('a request the server never answered is an error, not an empty result', () => {
    const out = pairBatch(reqs, [{ jsonrpc: '2.0', id: 2, result: null }])
    expect(out[0].error).toBeDefined()
    expect(out[1]).toEqual({ result: null })
    expect(out[2].error).toBeDefined()
  })

  test('a whole-batch refusal (single error object, null id) fails every request', () => {
    const out = pairBatch(reqs, { jsonrpc: '2.0', id: null, error: { code: -32600, message: 'too big' } })
    expect(out.every((o) => o.error)).toBe(true)
  })
})
