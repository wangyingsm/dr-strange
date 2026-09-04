import { describe, expect, test } from 'bun:test'
import { accept, cell, ghost, inStringLiteral, toTsv } from './cypher.js'

describe('completion', () => {
  test('completes the word being typed, keeping the keyword casing', () => {
    expect(ghost('MAT')).toBe('CH')
    expect(ghost('MATCH (n) RET')).toBe('URN')
    // The algorithm names read lower-case in a query, and complete that way.
    expect(ghost('CALL pager')).toBe('ank')
  })

  test('offers the shortest keyword, so the longer one stays reachable', () => {
    expect(ghost('MATCH')).toBe('ING') // MATCH → MATCHING
    expect(ghost('MATCHI')).toBe('NG')
  })

  test('guesses nothing from one letter, or from nothing', () => {
    expect(ghost('M')).toBe('')
    expect(ghost('')).toBe('')
    expect(ghost('MATCH ')).toBe('')
    expect(ghost('MATCH (n:Person) WHERE n.age >= 1')).toBe('')
  })

  test('says nothing inside a string, where words are data', () => {
    expect(inStringLiteral('SEARCH (d:Doc) NEAR "how do')).toBe(true)
    expect(inStringLiteral('SEARCH (d:Doc) NEAR "how do I"')).toBe(false)
    // `do` would otherwise be a prefix of nothing, but `MAT` here proves the
    // suppression rather than an accidental miss.
    expect(ghost('WHERE d.title = "MAT')).toBe('')
    expect(ghost("WHERE d.title = 'MAT")).toBe('')
  })
})

describe('results', () => {
  test('a cell shows what the value is, including absence', () => {
    expect(cell(null)).toBe('—')
    expect(cell(undefined)).toBe('—')
    expect(cell(0)).toBe('0')
    expect(cell(false)).toBe('false')
    expect(cell('crates/exec.rs')).toBe('crates/exec.rs')
    expect(cell([2019, 2021])).toBe('[2019,2021]')
  })

  test('a table copies as tab-separated text with its header', () => {
    const table = {
      columns: ['f.file', 'calls'],
      rows: [
        ['crates/exec.rs', 42],
        ['crates/compile.rs', 31],
      ],
    }
    expect(toTsv(table)).toBe(
      'f.file\tcalls\ncrates/exec.rs\t42\ncrates/compile.rs\t31',
    )
  })

  test('a cell that contains a tab or newline cannot break the columns', () => {
    const table = { columns: ['doc'], rows: [['two\tparts\nand a line']] }
    expect(toTsv(table)).toBe('doc\ntwo parts and a line')
  })
})

describe('accepting a completion', () => {
  test('splices at the caret and leaves the caret after it', () => {
    expect(accept('MATCH ', 6, '(n:Fn)')).toEqual({ text: 'MATCH (n:Fn)', caret: 12 })
  })

  test('keeps what follows a caret in the middle', () => {
    expect(accept('MATCH  RETURN n', 6, '(n:Fn)')).toEqual({
      text: 'MATCH (n:Fn) RETURN n',
      caret: 12,
    })
  })

  test('clamps a caret that is outside the text', () => {
    expect(accept('abc', 99, 'X')).toEqual({ text: 'abcX', caret: 4 })
    expect(accept('abc', -5, 'X')).toEqual({ text: 'Xabc', caret: 1 })
  })
})
