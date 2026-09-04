import { describe, expect, test } from 'bun:test'
import { formatVector, unwrapVector, vectorDims } from './vectors.js'

describe('recognising a vector', () => {
  test('a marker says how many dimensions without carrying them', () => {
    expect(vectorDims('$vector(1024 dims, omitted)')).toBe(1024)
    expect(vectorDims('$vector(3 dims, omitted)')).toBe(3)
  })

  test('a literal array still counts, for a caller that asked for one', () => {
    expect(vectorDims([0.1, 0.2, 0.3])).toBe(3)
  })

  test('anything else is not a vector', () => {
    expect(vectorDims('a doc')).toBeNull()
    expect(vectorDims('$vector(oops)')).toBeNull()
    expect(vectorDims(['a', 'b'])).toBeNull()
    expect(vectorDims([])).toBeNull()
    expect(vectorDims(42)).toBeNull()
    expect(vectorDims(null)).toBeNull()
  })
})

describe('unwrapping the floats', () => {
  test('reads them through either wrapping, or both', () => {
    expect(unwrapVector({ $vector: [1, 2] })).toEqual([1, 2])
    expect(unwrapVector({ $desc: 'the embedding', $value: { $vector: [1, 2] } })).toEqual([1, 2])
    expect(unwrapVector([1, 2])).toEqual([1, 2])
  })

  test('a marker holds no floats, which is the point of it', () => {
    expect(unwrapVector('$vector(3 dims, omitted)')).toBeNull()
    expect(unwrapVector({ $value: 'a doc' })).toBeNull()
    expect(unwrapVector(undefined)).toBeNull()
  })
})

describe('showing the floats', () => {
  test('six to a row, aligned, so a thousand of them read as a grid', () => {
    const rows = formatVector([1, 2, 3, 4, 5, 6, 7]).split('\n')
    expect(rows).toHaveLength(2)
    expect(rows[0]).toBe('   1.00000    2.00000    3.00000    4.00000    5.00000    6.00000')
    expect(rows[1]).toBe('   7.00000')
  })
})
