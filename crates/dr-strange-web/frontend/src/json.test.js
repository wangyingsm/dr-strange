import { describe, expect, test } from 'bun:test'
import { highlightJson } from './json.js'

describe('highlighting JSON', () => {
  test('a key is not a string, though both are quoted', () => {
    const html = highlightJson({ file: 'a.rs' })
    expect(html).toContain('<span class="j-key">&quot;file&quot;</span>')
    expect(html).toContain('<span class="j-str">&quot;a.rs&quot;</span>')
  })

  test('numbers and literals each have their own', () => {
    // What JSON.stringify writes is what is highlighted: `-1.5e3` is `-1500`
    // by the time it gets here, and only a number too big to spell keeps an
    // exponent.
    const html = highlightJson({ line: 480, ok: true, gone: null, x: -1.5e3, big: 1e21 })
    expect(html).toContain('<span class="j-num">480</span>')
    expect(html).toContain('<span class="j-lit">true</span>')
    expect(html).toContain('<span class="j-lit">null</span>')
    expect(html).toContain('<span class="j-num">-1500</span>')
    expect(html).toContain('<span class="j-num">1e+21</span>')
  })

  test('a property carrying markup is text, not markup', () => {
    const html = highlightJson({ doc: '<script>alert(1)</script> & "quoted"' })
    expect(html).not.toContain('<script>')
    expect(html).toContain('&lt;script&gt;')
    expect(html).toContain('&amp;')
  })

  test('a quote inside a string does not end it', () => {
    const html = highlightJson({ sig: 'fn f(s: &str) -> "x"' })
    expect(html).toContain('j-str')
    expect(html).not.toContain('j-key">&quot;x')
  })

  test('the shape survives: what it highlights still parses', () => {
    const value = { a: [1, 2, { b: 'c' }], d: null }
    const plain = highlightJson(value)
      .replace(/<[^>]+>/g, '')
      .replace(/&quot;/g, '"')
      .replace(/&amp;/g, '&')
      .replace(/&lt;/g, '<')
      .replace(/&gt;/g, '>')
    expect(JSON.parse(plain)).toEqual(value)
  })
})
