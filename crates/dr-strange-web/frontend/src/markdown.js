// A small Markdown renderer for the subset doc comments actually use.
//
// **Safe by construction, which is why it is hand-written.** The output goes
// through `{@html}`, and property values are not ours: they come from ingested
// documents, from a model's extraction, from whatever a plugin parsed. A
// general Markdown library passes raw HTML through by design, so using one here
// would mean pairing it with a sanitizer and trusting that pair on every
// upgrade. Instead every character is escaped *first* and only a fixed set of
// tags is ever emitted afterwards, so there is no path from input to markup.
//
// The subset is what Rust doc comments contain: headings, fenced and inline
// code, bold, italic, lists, block quotes and links. Anything else survives as
// the text it was, which is the right failure for a viewer.

const ESCAPES = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }
const escape = (s) => s.replace(/[&<>"]/g, (c) => ESCAPES[c])

// Only schemes that cannot execute. `javascript:` is the reason this exists;
// anything unrecognised renders as text rather than becoming a link.
const SAFE_URL = /^(https?:\/\/|mailto:|#|\/|\.\/|\.\.\/)/i

/** Markdown → HTML. The input is never trusted; the output is always escaped. */
export function renderMarkdown(source) {
  const text = String(source ?? '').replace(/\r\n?/g, '\n')

  // Fenced code comes out first, before anything can look inside it: a `*` in
  // a code sample is a `*`, not emphasis. The placeholders are NUL-delimited
  // because escaping has already removed every character that means anything
  // in markup, while a printable marker could occur in the text itself.
  const fences = []
  const withoutFences = text.replace(/```[^\n`]*\n([\s\S]*?)```/g, (_, body) => {
    fences.push(`<pre class="md-code"><code>${escape(body.replace(/\n$/, ''))}</code></pre>`)
    return `\u0000F${fences.length - 1}\u0000`
  })

  const lines = escape(withoutFences).split('\n')
  const out = []
  let list = null // 'ul' | 'ol' | null
  let quoting = false
  let para = []

  const closeParagraph = () => {
    if (para.length) {
      out.push(`<p>${inline(para.join(' '))}</p>`)
      para = []
    }
  }
  const closeList = () => {
    if (list) {
      out.push(`</${list}>`)
      list = null
    }
  }
  const closeQuote = () => {
    if (quoting) {
      out.push('</blockquote>')
      quoting = false
    }
  }
  const closeAll = () => {
    closeParagraph()
    closeList()
    closeQuote()
  }

  for (const raw of lines) {
    const line = raw.trimEnd()

    // A fence placeholder stands alone: it is already finished markup.
    const fence = line.match(/^\u0000F(\d+)\u0000$/)
    if (fence) {
      closeAll()
      out.push(fences[Number(fence[1])])
      continue
    }

    if (!line.trim()) {
      closeAll()
      continue
    }

    const heading = line.match(/^(#{1,6})\s+(.*)$/)
    if (heading) {
      closeAll()
      // Capped at h4: these are fragments inside a panel, not a document, and
      // an h1 here would out-shout the inspector's own headings.
      const level = Math.min(heading[1].length + 2, 6)
      out.push(`<h${level}>${inline(heading[2])}</h${level}>`)
      continue
    }

    if (/^(-{3,}|\*{3,}|_{3,})$/.test(line.trim())) {
      closeAll()
      out.push('<hr />')
      continue
    }

    const quote = line.match(/^&gt;\s?(.*)$/)
    if (quote) {
      closeParagraph()
      closeList()
      if (!quoting) {
        out.push('<blockquote>')
        quoting = true
      }
      out.push(`<p>${inline(quote[1])}</p>`)
      continue
    }
    closeQuote()

    const bullet = line.match(/^\s*[-*+]\s+(.*)$/)
    const numbered = line.match(/^\s*\d+[.)]\s+(.*)$/)
    if (bullet || numbered) {
      closeParagraph()
      const want = bullet ? 'ul' : 'ol'
      if (list !== want) {
        closeList()
        out.push(`<${want}>`)
        list = want
      }
      out.push(`<li>${inline((bullet ?? numbered)[1])}</li>`)
      continue
    }
    closeList()

    // Anything else is prose, joined until a blank line ends the paragraph —
    // which is what makes a doc comment's hard-wrapped lines read as sentences.
    para.push(line.trim())
  }
  closeAll()

  return out.join('\n')
}

/** Inline spans, in an order that keeps code contents literal. */
function inline(text) {
  const codes = []
  let s = text.replace(/`([^`]+)`/g, (_, code) => {
    codes.push(`<code class="md-inline">${code}</code>`)
    return `\u0000C${codes.length - 1}\u0000`
  })

  s = s.replace(/\[([^\]]*)\]\(([^)\s]+)\)/g, (whole, label, url) =>
    SAFE_URL.test(url)
      ? `<a href="${url}" target="_blank" rel="noreferrer noopener">${label || url}</a>`
      : whole,
  )

  s = s.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
  s = s.replace(/(^|[\s(])[*_]([^*_\n]+)[*_](?=$|[\s.,;:)])/g, '$1<em>$2</em>')

  return s.replace(/\u0000C(\d+)\u0000/g, (_, i) => codes[Number(i)])
}
