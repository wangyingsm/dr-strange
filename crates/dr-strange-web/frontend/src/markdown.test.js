import { expect, test } from 'bun:test'
import { renderMarkdown } from './markdown.js'

// ---- safety --------------------------------------------------------------
//
// The output goes through `{@html}` and the input is whatever was ingested, so
// these are the tests that matter most.

test('markup in the source becomes text, never markup', () => {
  const html = renderMarkdown('<script>alert(1)</script>')
  expect(html).not.toContain('<script>')
  expect(html).toContain('&lt;script&gt;')
})

test('an img with an onerror handler cannot survive', () => {
  const html = renderMarkdown('<img src=x onerror="alert(1)">')
  expect(html).not.toContain('<img')
  expect(html).toContain('&lt;img')
})

test('a javascript: link renders as text rather than a link', () => {
  const html = renderMarkdown('[click](javascript:alert(1))')
  expect(html).not.toContain('href="javascript')
  expect(html).toContain('[click]')
})

test('an http link is a link, and opens away from the app', () => {
  const html = renderMarkdown('see [the paper](https://example.com/p)')
  expect(html).toContain('<a href="https://example.com/p"')
  expect(html).toContain('rel="noreferrer noopener"')
  expect(html).toContain('>the paper</a>')
})

test('an attribute cannot be broken out of', () => {
  const html = renderMarkdown('[x](https://a.test/" onmouseover="alert(1))')
  expect(html).not.toContain('onmouseover="alert')
})

// ---- the subset doc comments use -----------------------------------------

test('hard-wrapped lines join into one paragraph', () => {
  const html = renderMarkdown('Public API layer:\n`Database`, `PlaneHandle`,\nand the builder.')
  expect(html.match(/<p>/g)).toHaveLength(1)
  expect(html).toContain('<code class="md-inline">Database</code>')
})

test('a blank line separates paragraphs', () => {
  expect(renderMarkdown('one\n\ntwo').match(/<p>/g)).toHaveLength(2)
})

test('headings render, capped so they do not out-shout the panel', () => {
  expect(renderMarkdown('# Title')).toContain('<h3>Title</h3>')
  expect(renderMarkdown('## Sub')).toContain('<h4>Sub</h4>')
})

test('a fenced block keeps its contents literal', () => {
  const html = renderMarkdown('before\n\n```rust\nlet a = *b * *c;\n```\n\nafter')
  expect(html).toContain('<pre class="md-code"><code>let a = *b * *c;</code></pre>')
  // The asterisks inside code are not emphasis.
  expect(html).not.toContain('<em>')
})

test('code inside a fence is escaped too', () => {
  expect(renderMarkdown('```\n<b>x</b>\n```')).toContain('&lt;b&gt;x&lt;/b&gt;')
})

test('bullets and numbers become lists', () => {
  const ul = renderMarkdown('- one\n- two')
  expect(ul).toContain('<ul>')
  expect(ul.match(/<li>/g)).toHaveLength(2)
  expect(renderMarkdown('1. one\n2. two')).toContain('<ol>')
})

test('emphasis, but not inside an identifier', () => {
  expect(renderMarkdown('**bold** and *italic*')).toContain('<strong>bold</strong>')
  expect(renderMarkdown('**bold** and *italic*')).toContain('<em>italic</em>')
  // `snake_case_name` is a name, not emphasis — the commonest false positive
  // in a doc comment about code.
  expect(renderMarkdown('a snake_case_name here')).not.toContain('<em>')
})

test('a block quote is quoted, not escaped into prose', () => {
  const html = renderMarkdown('> quoted line')
  expect(html).toContain('<blockquote>')
  expect(html).toContain('quoted line')
})

test('an intra-doc link keeps its brackets rather than vanishing', () => {
  const html = renderMarkdown('see [`QueryBuilder`] for more')
  expect(html).toContain('[<code class="md-inline">QueryBuilder</code>]')
})

test('plain text with no markup at all still renders', () => {
  expect(renderMarkdown('src')).toBe('<p>src</p>')
})

test('an empty or missing value renders nothing', () => {
  expect(renderMarkdown('')).toBe('')
  expect(renderMarkdown(null)).toBe('')
})

test('a placeholder-looking string in the text is not mistaken for one', () => {
  // The markers are NUL-delimited precisely so this cannot collide.
  const html = renderMarkdown('the C0 register and F0 flag')
  expect(html).toContain('the C0 register and F0 flag')
})
