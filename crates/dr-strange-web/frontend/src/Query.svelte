<script>
  import { authHeaders, rpc } from './rpc.js'
  import { loadPref, savePref } from './prefs.js'
  import { accept, cell, ghost, toTsv } from './cypher.js'
  import { formatVector, unwrapVector, vectorDims } from './vectors.js'
  import { labelColor } from './labels.js'
  import { highlightJson } from './json.js'

  // `plane` is the app-wide current plane, which a query names none of its own.
  let { plane } = $props()

  // Providers with an embedding endpoint, for a text `SEARCH … NEAR "…"`
  // (deepseek is chat-only, so excluded) — the same list the header search offers.
  const EMBED_PROVIDERS = ['openai', 'qwen', 'ollama']

  const EXAMPLES = [
    ['Count by group', 'MATCH (f:Fn)-[:CALLS]->(g:Fn)\nRETURN f.file, count(*) AS calls\nORDER BY calls DESC\nLIMIT 10'],
    ['Whole records', 'MATCH (n:Fn)\nWHERE n.file ENDS WITH "exec.rs"\nRETURN n\nLIMIT 25'],
    ['Across a hop', 'MATCH (a)-[:CALLS]->(b:Fn)\nRETURN key(a) AS caller, b.name AS callee\nLIMIT 50'],
    ['Past snapshot', 'MATCH (n:Fn) RETURN count(*) AS fns AS OF 1'],
  ]

  // How long typing must pause before the server is asked what comes next.
  // A round trip is not free, and a guess that arrives while the next
  // character is being typed was never worth making.
  const IDLE_MS = 1000

  // The box opens empty, every time: a query is written for one plane's
  // labels and would fail on most others, so a statement waiting in the box
  // is a statement someone has to read and delete before they can start. The
  // placeholder says the shape instead, and the examples below write a real
  // one on request.
  let text = $state('')
  // Where the caret is, and the textarea itself — a completion is for the
  // text *before* the caret, and accepting one has to put the caret after it.
  let caret = $state(text.length)
  let box = $state(null)
  // The server's last answer: `{ prefix, plane, best, about, suggestions }`.
  let guess = $state(null)
  let provider = $state(loadPref('embedProvider', 'openai'))
  let busy = $state(false)
  let error = $state(null)
  let result = $state(null) // { table } | { records } | { write } | null
  let elapsed = $state(null) // ms of the last run, so a slow query says so
  let copied = $state(false)
  // Which page of the answer is on screen. A query says how many rows it
  // matched; a reader looks at a screenful, and the two stopped being the
  // same number the first time a pattern matched four thousand functions.
  const PAGE = 200
  let offset = $state(0)

  // What a completion is for: the query up to the caret.
  const prefix = $derived(text.slice(0, caret))
  // The server's answer, but only while it still describes what is typed in
  // the plane it was asked about — one keystroke later, or one plane over, it
  // is about a query that no longer exists.
  const remote = $derived(guess?.prefix === prefix && guess?.plane === plane ? guess : null)
  // The ghost is drawn after the caret, so it can only be drawn when the
  // caret is at the end; mid-text, the list below is the whole story.
  const atEnd = $derived(caret >= text.length)
  // Once the server has spoken about this exact prefix its word is final,
  // including when the word is "nothing" — inside a string literal, say. The
  // keyword ghost fills the second before it answers, so the box is never
  // dead while a key is still warm.
  const completion = $derived(!atEnd ? '' : remote ? (remote.best ?? '') : ghost(prefix))
  // Not gated on focus: the row holds its line whether or not it has anything
  // in it, because a line that comes and goes moves the Run button out from
  // under the cursor about to click it.
  const suggestions = $derived(remote?.suggestions ?? [])

  // Ask on a pause, and only about what is actually typed now.
  let timer
  let inflight
  $effect(() => {
    const asked = prefix
    const on = plane
    clearTimeout(timer)
    timer = setTimeout(() => ask(asked, on), IDLE_MS)
    return () => clearTimeout(timer)
  })

  async function ask(asked, on) {
    inflight?.abort()
    const ctrl = new AbortController()
    inflight = ctrl
    try {
      // POST /cypher/complete is web-only, like /cypher: a raw fetch carrying
      // the bearer token the RPC client would add.
      const res = await fetch(`/cypher/complete?plane=${encodeURIComponent(on)}`, {
        method: 'POST',
        headers: authHeaders({ 'content-type': 'text/plain' }),
        body: asked,
        signal: ctrl.signal,
      })
      if (!res.ok) return
      guess = { prefix: asked, plane: on, ...(await res.json()) }
    } catch {
      // Advisory: a guess that never arrives is no worse than no guess, and
      // an aborted one is a guess we no longer wanted.
    }
  }

  // The caret drives everything, so it is read back from the DOM after
  // anything that could have moved it.
  function track(e) {
    caret = e.currentTarget.selectionStart ?? 0
  }

  /** Take a suggestion: splice it in, and leave the caret after it. */
  function apply(insert) {
    const out = accept(text, caret, `${insert} `)
    text = out.text
    caret = out.caret
    // The DOM caret has to follow, or the next keystroke lands where the old
    // one was.
    queueMicrotask(() => {
      box?.focus()
      box?.setSelectionRange(out.caret, out.caret)
    })
  }

  // Ctrl/Cmd+Enter runs; plain Enter is a newline. Tab takes the completion
  // when there is one and otherwise moves focus, so a keyboard user can leave
  // the box.
  function onKey(e) {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault()
      run()
    } else if (e.key === 'Tab' && completion) {
      e.preventDefault()
      apply(completion)
    }
  }

  /** Run from the top — what the Run button and ⌘/Ctrl+Enter do. */
  function run() {
    return runAt(0)
  }

  // POST /cypher is web-only, not an RPC method: a raw fetch carrying the
  // bearer token the RPC client would add.
  async function ask_cypher({ offset: at, limit, lean }) {
    const params = new URLSearchParams({
      plane,
      embed: provider,
      offset: String(at),
      limit: String(limit),
    })
    if (lean === false) params.set('lean', 'false')
    const res = await fetch(`/cypher?${params}`, {
      method: 'POST',
      headers: authHeaders({ 'content-type': 'text/plain' }),
      body: text.trim(),
    })
    if (!res.ok) throw new Error((await res.text()) || `query failed (${res.status})`)
    return res.json()
  }

  async function runAt(at) {
    const query = text.trim()
    if (!query || busy) return
    offset = at
    busy = true
    error = null
    copied = false
    savePref('embedProvider', provider)
    const started = performance.now()
    try {
      const out = await ask_cypher({ offset: at, limit: PAGE })
      elapsed = Math.round(performance.now() - started)
      // Three shapes from one endpoint: columns, nodes, or change counts.
      if (out.columns) result = { table: out }
      else if (out.write) result = { write: out }
      else result = { records: out }
    } catch (e) {
      error = e.message
      result = null
    } finally {
      busy = false
    }
  }

  function copy() {
    if (!result?.table) return
    navigator.clipboard.writeText(toTsv(result.table))
    copied = true
    setTimeout(() => (copied = false), 1200)
  }

  // The node's embedding, if it has one: which property, and how big. Its own
  // column, because every node in a digested plane has one and it says the
  // same thing on every row — down a column it is a glance, in the middle of
  // the properties it is noise.
  //
  // Not named `props`: a local binding by that name makes `$props` read as a
  // store subscription, which Svelte warns about.
  function vectorOf(node) {
    for (const [k, v] of Object.entries(node.properties ?? {})) {
      const dims = vectorDims(v)
      if (dims) return { k, dims }
    }
    return null
  }

  // The rest, briefly: enough to tell two rows apart. Provenance (`_`-prefixed)
  // and the embedding are left out — the first is retrieval's business, the
  // second has its own column — and everything is in the popup anyway.
  function brief(node) {
    return Object.entries(node.properties ?? {})
      .filter(([k, v]) => v !== null && typeof v !== 'object' && !k.startsWith('_') && !vectorDims(v))
      .slice(0, 3)
      .map(([k, v]) => `${k}: ${clip(String(v))}`)
      .join(' · ')
  }

  const clip = (s) => (s.length > 48 ? `${s.slice(0, 47)}…` : s)

  // The whole property map of one node, JSON and all.
  let propsView = $state(null)

  // The floats behind a marker, fetched for the one row whose button was
  // pressed. `lean: false` is the only way to them, and one row at a time is
  // the whole point of asking.
  //
  // `token` says which button opened this: a later click supersedes an earlier
  // one, and a slow answer to a question nobody is asking any more is dropped
  // rather than shown.
  let vectorView = $state(null) // { title, dims, values, error, token } | null

  async function fetching(token, title, dims, get) {
    vectorView = { token, title, dims, values: null }
    try {
      const values = await get()
      if (!values) throw new Error('no vector came back')
      if (vectorView?.token === token) {
        vectorView = { token, title, dims: values.length, values }
      }
    } catch (e) {
      if (vectorView?.token === token) {
        vectorView = { token, title, dims, values: null, error: e.message }
      }
    }
  }

  /** A node's embedding, by id — the records table. */
  function showVector(node, k, dims) {
    return fetching(`node:${node.id}:${k}`, k, dims, async () => {
      const whole = await rpc('node.get', { plane, id: node.id, lean: false })
      return unwrapVector(whole?.properties?.[k])
    })
  }

  /** A projected column's embedding — the table of a `RETURN m.embedding`.
   *
   * A projection carries no node to ask about, so the question is put the way
   * it was first asked: the same query, at that one row, with the vectors
   * left in. */
  function showCellVector(r, col, dims) {
    const at = offset + r
    return fetching(`row:${at}:${col}`, result?.table?.columns[col] ?? 'vector', dims, async () => {
      const out = await ask_cypher({ offset: at, limit: 1, lean: false })
      return unwrapVector(out?.rows?.[0]?.[col])
    })
  }

  // What the header says about a paged answer: which rows these are, of how
  // many there were.
  // Rows on this page, not distinct nodes: a pattern can reach one node by
  // several paths, and the page is of rows.
  const shown = $derived(result?.table?.rows.length ?? result?.records?.nodes.length ?? 0)
  const total = $derived(result?.table?.total ?? result?.records?.total ?? shown)
  const pageOf = $derived(
    total > shown ? `${offset + 1}–${offset + shown} of ${total}` : `${total}`,
  )
  const morePages = $derived(offset + shown < total)

  const changes = (w) =>
    [
      [w.nodes_created, 'nodes created'],
      [w.edges_created, 'edges created'],
      [w.props_set, 'props set'],
      [w.labels_set, 'labels set'],
      [w.nodes_deleted, 'nodes deleted'],
      [w.edges_deleted, 'edges deleted'],
    ]
      .filter(([n]) => n > 0)
      .map(([n, label]) => `${n} ${label}`)
</script>

<section class="query-page">
  <div class="q-editor">
    <div class="q-input-wrap">
      <!-- Behind the textarea with identical typography, so the completion
           lines up after the caret at any wrap point. -->
      <div class="q-ghost" aria-hidden="true"><span class="typed">{text}</span>{completion}{#if completion}<span class="tab-key">Tab</span>{/if}</div>
      <textarea
        class="q-text"
        bind:this={box}
        bind:value={text}
        onkeydown={onKey}
        oninput={track}
        onkeyup={track}
        onclick={track}
        onfocus={track}
        spellcheck="false"
        rows="6"
        placeholder="MATCH (n:Label)-[:TYPE]->(m) WHERE n.name = &quot;…&quot; RETURN n.name, count(*) AS c"
        aria-label="Query"
      ></textarea>
    </div>
    <!-- What this plane would make of the caret: the candidates, each with the
         count that ranked it. The row is always here, holding its line even
         when empty; `mousedown` is swallowed so clicking one does not blur the
         box out from under the click. -->
    <div class="q-suggest">
      {#if suggestions.length}
        <span class="q-suggest-about">{remote.about}</span>
        {#each suggestions as s (s.text + s.insert)}
          <button class="q-sug" onmousedown={(e) => e.preventDefault()} onclick={() => apply(s.insert)}>
            <span class="q-sug-text">{s.text}</span>
            {#if s.detail}<span class="q-sug-detail">{s.detail}</span>{/if}
          </button>
        {/each}
      {/if}
    </div>
    <div class="q-actions">
      <button class="run" onclick={run} disabled={busy}>{busy ? 'Running…' : 'Run'}</button>
      <span class="hint"><kbd>⌘</kbd>/<kbd>Ctrl</kbd>+<kbd>Enter</kbd></span>
      <span class="grow"></span>
      <span class="q-plane">plane <b>{plane}</b></span>
      <label>
        embed
        <select bind:value={provider} title="Provider for a text SEARCH … NEAR &quot;…&quot;">
          {#each EMBED_PROVIDERS as p (p)}<option value={p}>{p}</option>{/each}
        </select>
      </label>
    </div>
    <div class="q-examples">
      {#each EXAMPLES as [label, body] (label)}
        <button onclick={() => (text = body)} title={body}>{label}</button>
      {/each}
    </div>
  </div>

  {#if error}
    <p class="q-error">{error}</p>
  {/if}

  {#if result?.table}
    <div class="q-result">
      <header>
        <span>
          {pageOf} row{total === 1 ? '' : 's'} ·
          {result.table.columns.length} column{result.table.columns.length === 1 ? '' : 's'}
          {#if elapsed != null} · {elapsed} ms{/if}
        </span>
        <span class="q-pager">
          {#if offset > 0 || morePages}
            <button onclick={() => runAt(Math.max(0, offset - PAGE))} disabled={busy || offset === 0}>‹ prev</button>
            <button onclick={() => runAt(offset + PAGE)} disabled={busy || !morePages}>next ›</button>
          {/if}
          <button class="copy" onclick={copy}>{copied ? 'copied' : 'copy'}</button>
        </span>
      </header>
      <div class="q-scroll">
        <table>
          <thead>
            <tr>{#each result.table.columns as c, i (`${c}:${i}`)}<th>{c}</th>{/each}</tr>
          </thead>
          <tbody>
            {#each result.table.rows as row, r (r)}
              <tr>
                {#each row as v, i (i)}
                  <td>
                    {#if vectorDims(v)}
                      <button class="vec-btn" onclick={() => showCellVector(r, i, vectorDims(v))}>
                        {vectorDims(v)} dims
                      </button>
                    {:else}{cell(v)}{/if}
                  </td>
                {/each}
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {#if !result.table.rows.length}
        <p class="q-empty">No rows. The query ran; nothing matched it.</p>
      {/if}
    </div>
  {:else if result?.records}
    <div class="q-result">
      <header>
        <span>
          {pageOf} node{total === 1 ? '' : 's'} ·
          {result.records.edges.length} edge{result.records.edges.length === 1 ? '' : 's'}
          {#if elapsed != null} · {elapsed} ms{/if}
        </span>
        {#if offset > 0 || morePages}
          <span class="q-pager">
            <button onclick={() => runAt(Math.max(0, offset - PAGE))} disabled={busy || offset === 0}>‹ prev</button>
            <button onclick={() => runAt(offset + PAGE)} disabled={busy || !morePages}>next ›</button>
          </span>
        {/if}
      </header>
      <div class="q-scroll">
        <table>
          <thead>
            <tr>
              <th>key</th><th>labels</th><th>embedding</th><th>properties</th>
            </tr>
          </thead>
          <tbody>
            <!-- Keyed by position, not by id: the server sends each node once,
                 and a table that wedges when it does not is a worse table. -->
            {#each result.records.nodes as n, r (r)}
              <tr>
                <td>{n.external_key ?? `#${n.id}`}</td>
                <td class="q-labels">
                  {#each n.labels ?? [] as l (l)}
                    <span class="q-chip" style="--chip: {labelColor(l)}">{l}</span>
                  {/each}
                </td>
                <td class="q-vec">
                  {#if vectorOf(n)}
                    {@const vec = vectorOf(n)}
                    <button class="vec-btn" onclick={() => showVector(n, vec.k, vec.dims)}>
                      {vec.dims} dims
                    </button>
                  {:else}
                    <span class="q-none">—</span>
                  {/if}
                </td>
                <td class="q-props">
                  <span class="q-brief">{brief(n)}</span>
                  <button
                    class="q-expand"
                    onclick={() => (propsView = n)}
                    title="Show every property"
                  >
                    <span class="q-braces" aria-hidden="true">{'{ }'}</span> expand
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {#if !result.records.count}
        <p class="q-empty">No nodes. The query ran; nothing matched it.</p>
      {/if}
    </div>
  {:else if result?.write}
    <div class="q-result">
      <header><span>Write{#if elapsed != null} · {elapsed} ms{/if}</span></header>
      <p class="q-write">{changes(result.write).join(' · ') || 'no changes'}</p>
    </div>
  {/if}

  {#if propsView}
    <div class="modal-backdrop">
      <div class="modal">
        <header>
          <span>{propsView.external_key ?? `#${propsView.id}`} · properties</span>
          <button class="close" onclick={() => (propsView = null)} aria-label="Close">×</button>
        </header>
        <pre class="floats j-code">{@html highlightJson(propsView.properties ?? {})}</pre>
      </div>
    </div>
  {/if}

  {#if vectorView}
    <!-- The same popup the Explore inspector opens: a thousand floats read as
         a grid, and only ever for the node whose button was pressed. -->
    <div class="modal-backdrop">
      <div class="modal">
        <header>
          <span>{vectorView.title} · {vectorView.dims} dims</span>
          <button class="close" onclick={() => (vectorView = null)} aria-label="Close">×</button>
        </header>
        {#if vectorView.values}
          <pre class="floats">{formatVector(vectorView.values)}</pre>
        {:else}
          <pre class="floats">{vectorView.error ?? 'fetching…'}</pre>
        {/if}
      </div>
    </div>
  {/if}
</section>
