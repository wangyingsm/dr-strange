import { describe, expect, test } from 'bun:test'
import { PALETTE, labelColor } from './labels.js'

describe('label colours', () => {
  test('the same label is the same colour, every time and every page', () => {
    expect(labelColor('Function')).toBe(labelColor('Function'))
    expect(PALETTE).toContain(labelColor('Function'))
  })

  test('different labels generally differ', () => {
    const labels = ['Function', 'Method', 'Struct', 'Module', 'UnresolvedRef', 'External']
    const colours = new Set(labels.map(labelColor))
    expect(colours.size).toBeGreaterThan(3)
  })

  test('anything nameable has a colour, including nothing much', () => {
    expect(PALETTE).toContain(labelColor(''))
    expect(PALETTE).toContain(labelColor('日本語'))
  })
})
