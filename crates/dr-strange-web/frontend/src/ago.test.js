import { describe, expect, test } from 'bun:test'
import { ago } from './ago.js'

const NOW = Date.UTC(2026, 8, 5, 12, 0, 0)
const before = (ms) => ago(NOW - ms, NOW)

describe('how long ago', () => {
  test('the last half minute is just now', () => {
    expect(before(0)).toBe('just now')
    expect(before(44_000)).toBe('just now')
  })

  test('minutes, then hours, then days', () => {
    expect(before(60_000)).toBe('1m ago')
    expect(before(45 * 60_000)).toBe('45m ago')
    expect(before(3 * 3_600_000)).toBe('3h ago')
    expect(before(3 * 86_400_000)).toBe('3d ago')
  })

  test('past a month, the date is more use than the count', () => {
    expect(before(60 * 86_400_000)).toBe('2026-07-07')
  })

  test('a clock that runs ahead reads as just now, not backwards', () => {
    expect(ago(NOW + 5_000, NOW)).toBe('just now')
  })
})
