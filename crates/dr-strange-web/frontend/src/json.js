// JSON, coloured the way a reader expects to see it.
//
// Hand-rolled rather than a highlighter dependency, for the same reason
// `markdown.js` is: the grammar is four token kinds, the input is never
// trusted, and a syntax-highlighting library is three hundred kilobytes to
// tell a string from a number.
//
// The palette is GitHub's (Primer), so the popup reads like the file it came
// from — see `.j-*` in `app.css` for the light and dark values.

const ESCAPES = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }
const escape = (s) => s.replace(/[&<>"]/g, (c) => ESCAPES[c])

// A string (with the `:` that would make it a key), a literal, or a number.
// Everything between matches is punctuation and whitespace, which stays plain.
const TOKEN =
  /(?<str>"(?:\\.|[^"\\])*")(?<colon>\s*:)?|(?<lit>\btrue\b|\bfalse\b|\bnull\b)|(?<num>-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)/g

/**
 * A value as pretty-printed, highlighted HTML.
 *
 * Every piece is escaped on its way out — including inside strings, which is
 * where a property's own text arrives — so the result is safe to `{@html}`.
 */
export function highlightJson(value) {
  const text = JSON.stringify(value, null, 2) ?? 'null'
  let out = ''
  let last = 0
  for (const m of text.matchAll(TOKEN)) {
    const { str, colon, lit, num } = m.groups
    out += escape(text.slice(last, m.index))
    if (str !== undefined) {
      out += colon
        ? `<span class="j-key">${escape(str)}</span>${escape(colon)}`
        : `<span class="j-str">${escape(str)}</span>`
    } else if (lit !== undefined) {
      out += `<span class="j-lit">${escape(lit)}</span>`
    } else {
      out += `<span class="j-num">${escape(num)}</span>`
    }
    last = m.index + m[0].length
  }
  return out + escape(text.slice(last))
}
