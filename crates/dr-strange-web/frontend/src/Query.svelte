<script>
  import { authHeaders } from './rpc.js'
  import { loadPref, savePref } from './prefs.js'
  import { cell, ghost, toTsv } from './cypher.js'

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

  let text = $state(loadPref('queryText', EXAMPLES[0][1]))
  let provider = $state(loadPref('embedProvider', 'openai'))
  let busy = $state(false)
  let error = $state(null)
  let result = $state(null) // { table } | { records } | { write } | null
  let elapsed = $state(null) // ms of the last run, so a slow query says so
  let copied = $state(false)

  const completion = $derived(ghost(text))

  // Ctrl/Cmd+Enter runs; plain Enter is a newline. Tab takes the completion
  // when there is one and otherwise moves focus, so a keyboard user can leave
  // the box.
  function onKey(e) {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault()
      run()
    } else if (e.key === 'Tab' && completion) {
      e.preventDefault()
      text = text + completion + ' '
    }
  }

  async function run() {
    const query = text.trim()
    if (!query || busy) return
    busy = true
    error = null
    copied = false
    savePref('queryText', text)
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

  // A returned node as a row: key, labels, and the properties that fit.
  //
  // Not named `props`: a local binding by that name makes `$props` read as a
  // store subscription, which Svelte warns about.
  function propSummary(node) {
    return Object.entries(node.properties ?? {})
      .filter(([, v]) => v !== null && typeof v !== 'object')
      .slice(0, 4)
      .map(([k, v]) => `${k}: ${v}`)
      .join(' · ')
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
        bind:value={text}
        onkeydown={onKey}
        spellcheck="false"
        rows="6"
        placeholder="MATCH (n:Label) RETURN n.name, count(*) AS n"
        aria-label="Query"
      ></textarea>
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
                <td class="q-props">{propSummary(n)}</td>
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
</section>
