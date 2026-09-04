<script>
  import { authHeaders, rpc } from './rpc.js'
  import { loadPref, savePref } from './prefs.js'
  import { accept, cell, ghost, toTsv } from './cypher.js'
  import { formatVector, unwrapVector, vectorDims } from './vectors.js'

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
  let focused = $state(false)
  // The server's last answer: `{ prefix, plane, best, about, suggestions }`.
  let guess = $state(null)
  let provider = $state(loadPref('embedProvider', 'openai'))
  let busy = $state(false)
  let error = $state(null)
  let result = $state(null) // { table } | { records } | { write } | null
  let elapsed = $state(null) // ms of the last run, so a slow query says so
  let copied = $state(false)

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
  const suggestions = $derived(focused && remote ? remote.suggestions : [])

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

  async function run() {
    const query = text.trim()
    if (!query || busy) return
    busy = true
    error = null
    copied = false
    savePref('embedProvider', provider)
    const started = performance.now()
    try {
      // POST /cypher is web-only, not an RPC method: a raw fetch carrying the
      // bearer token the RPC client would add.
      const url = `/cypher?plane=${encodeURIComponent(plane)}&embed=${encodeURIComponent(provider)}`
      const res = await fetch(url, {
        method: 'POST',
        headers: authHeaders({ 'content-type': 'text/plain' }),
        body: query,
      })
      if (!res.ok) throw new Error((await res.text()) || `query failed (${res.status})`)
      const out = await res.json()
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

  // A returned node's properties, as many as fit in a row. A vector is one of
  // them — it arrives as a marker, so it reads as a button carrying its
  // dimension rather than as prose about being omitted.
  //
  // Not named `props`: a local binding by that name makes `$props` read as a
  // store subscription, which Svelte warns about.
  function propSummary(node) {
    return Object.entries(node.properties ?? {})
      .filter(([, v]) => v !== null && typeof v !== 'object')
      .slice(0, 4)
      .map(([k, v]) => ({ k, v, dims: vectorDims(v) }))
  }

  // The floats behind a marker, fetched for the one node whose button was
  // pressed — `lean: false` is the only way to them, and one node at a time is
  // the whole point of asking.
  let vectorView = $state(null) // { k, dims, values, error } | null

  async function showVector(node, k, dims) {
    vectorView = { k, dims, values: null }
    try {
      const whole = await rpc('node.get', { plane, id: node.id, lean: false })
      const values = unwrapVector(whole?.properties?.[k])
      if (!values) throw new Error('no vector came back')
      // A later click may have moved on; only the open one is ours to fill.
      if (vectorView?.k === k) vectorView = { k, dims: values.length, values }
    } catch (e) {
      if (vectorView?.k === k) vectorView = { k, dims, values: null, error: e.message }
    }
  }

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
        onfocus={(e) => {
          focused = true
          track(e)
        }}
        onblur={() => (focused = false)}
        spellcheck="false"
        rows="6"
        placeholder="MATCH (n:Label)-[:TYPE]->(m) WHERE n.name = &quot;…&quot; RETURN n.name, count(*) AS c"
        aria-label="Query"
      ></textarea>
    </div>
    {#if suggestions.length}
      <!-- What this plane would make of the caret: the candidates, each with
           the count that ranked it. `mousedown` is swallowed so clicking one
           does not blur the box out from under the click. -->
      <div class="q-suggest">
        <span class="q-suggest-about">{remote.about}</span>
        {#each suggestions as s (s.text + s.insert)}
          <button class="q-sug" onmousedown={(e) => e.preventDefault()} onclick={() => apply(s.insert)}>
            <span class="q-sug-text">{s.text}</span>
            {#if s.detail}<span class="q-sug-detail">{s.detail}</span>{/if}
          </button>
        {/each}
      </div>
    {/if}
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
          {result.table.rows.length} row{result.table.rows.length === 1 ? '' : 's'} ·
          {result.table.columns.length} column{result.table.columns.length === 1 ? '' : 's'}
          {#if elapsed != null} · {elapsed} ms{/if}
        </span>
        <button class="copy" onclick={copy}>{copied ? 'copied' : 'copy'}</button>
      </header>
      <div class="q-scroll">
        <table>
          <thead>
            <tr>{#each result.table.columns as c, i (`${c}:${i}`)}<th>{c}</th>{/each}</tr>
          </thead>
          <tbody>
            {#each result.table.rows as row, r (r)}
              <tr>{#each row as v, i (i)}<td>{cell(v)}</td>{/each}</tr>
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
          {result.records.count} node{result.records.count === 1 ? '' : 's'} ·
          {result.records.edges.length} edge{result.records.edges.length === 1 ? '' : 's'}
          {#if elapsed != null} · {elapsed} ms{/if}
        </span>
      </header>
      <div class="q-scroll">
        <table>
          <thead><tr><th>key</th><th>labels</th><th>properties</th></tr></thead>
          <tbody>
            {#each result.records.nodes as n (n.id)}
              <tr>
                <td>{n.external_key ?? `#${n.id}`}</td>
                <td>{(n.labels ?? []).join(', ')}</td>
                <td class="q-props">
                  {#each propSummary(n) as pe, i (pe.k)}
                    {#if i > 0}<span class="q-prop-sep"> · </span>{/if}
                    {#if pe.dims}
                      <button class="vec-btn" onclick={() => showVector(n, pe.k, pe.dims)}>
                        {pe.k} ({pe.dims} dims)
                      </button>
                    {:else}
                      <span>{pe.k}: {pe.v}</span>
                    {/if}
                  {/each}
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

  {#if vectorView}
    <!-- The same popup the Explore inspector opens: a thousand floats read as
         a grid, and only ever for the node whose button was pressed. -->
    <div class="modal-backdrop">
      <div class="modal">
        <header>
          <span>{vectorView.k} · {vectorView.dims} dims</span>
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
