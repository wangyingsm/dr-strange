<script>
  import { rpc, authHeaders } from './rpc.js'
  import { loadPref, savePref } from './prefs.js'
  import CreatePlane from './CreatePlane.svelte'

  const PROVIDERS = ['openai', 'deepseek', 'qwen', 'ollama']

  // The target plane comes from the app-wide picker in the header.
  // `onPlaneCreated` bubbles a new plane up to App (refresh picker + switch).
  let { plane, onPlaneCreated = () => {} } = $props()
  let newPlaneOpen = $state(false) // new-plane popup open?
  let text = $state('')
  let chat = $state(loadPref('digestChat', 'openai'))
  let embed = $state(loadPref('digestEmbed', 'openai'))

  // Remember the provider selections across reloads.
  $effect(() => {
    savePref('digestChat', chat)
    savePref('digestEmbed', embed)
  })
  // How much clean-up follows extraction (ROADMAP §8). Remembered like the
  // provider choices, since it is a standing preference rather than a per-run
  // decision.
  let mode = $state(loadPref('digestMode', 'fine'))
  const MODES = [
    ['coarse', 'reconcile the label and edge-type vocabularies only'],
    ['fine', 'also merge entities that name the same thing — the default'],
    [
      'super',
      're-read every entity against all the passages mentioning it: the most accurate, and ~15× the input token usage',
    ],
  ]
  let noEmbed = $state(false)
  let link = $state(true)
  // URL ingestion (ROADMAP §9). `pages` is what the crawl found, each with its
  // own Markdown block and relevance score; ticking one puts it in the text.
  let url = $state('')
  let topic = $state('')
  // 0 reads only the page named. Remembered like the provider choices: how far
  // a reader wants to follow links is a habit, not a per-page decision.
  let depth = $state(Number(loadPref('digestDepth', '0')))
  let pages = $state([])
  let dropped = $state([])
  let proposal = $state(null) // { report, nodes, edges }
  let status = $state('')
  let error = $state(null)
  let busy = $state(false)
  // Crawl progress 0..100 — pages are countable, so that one gets a real bar.
  // null means no bar; document conversion uses the overlay above instead.
  let pct = $state(null)
  // Work whose duration cannot be known: the LLM call, and converting an
  // uploaded document. Both raise the same centred spinning-logo overlay —
  // the message under it says which. A bar would have to invent a percentage.
  let thinking = $state(false)

  /// Both streaming endpoints answer newline-delimited JSON: zero or more
  /// progress lines, then a final result or `{error}`. Read it line by line so
  /// the progress bar moves during a slow extraction or crawl.
  async function stream(res, onMsg) {
    // A non-2xx here is a pre-stream failure (e.g. a 413 when the upload is too
    // large, or a 403 when fetching is disabled) with a non-streamed body.
    if (!res.ok) {
      const raw = await res.text()
      let msg = raw
      try {
        msg = JSON.parse(raw).error || raw
      } catch {
        /* keep raw */
      }
      throw new Error(msg || `request failed (${res.status})`)
    }
    const reader = res.body.getReader()
    const decoder = new TextDecoder()
    let buffer = ''
    let done = null
    for (;;) {
      const { value, done: streamDone } = await reader.read()
      if (streamDone) break
      buffer += decoder.decode(value, { stream: true })
      let nl
      while ((nl = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, nl).trim()
        buffer = buffer.slice(nl + 1)
        if (!line) continue
        const msg = JSON.parse(line)
        if (msg.error) throw new Error(msg.error)
        const result = onMsg(msg)
        if (result !== undefined) done = result
      }
    }
    return done
  }

  // Joining the ticked pages *here* rather than server-side is what makes the
  // selection interactive: unticking a page costs no request.
  function assemble() {
    text = pages
      .filter((p) => p.kept)
      .map((p) => p.block)
      .join('\n\n')
    proposal = null
  }

  async function fetchUrl() {
    if (!url.trim()) return
    error = null
    busy = true
    pct = null
    pages = []
    dropped = []
    status = `fetching ${url}…`
    try {
      const params = new URLSearchParams({ url: url.trim(), depth: String(depth) })
      if (topic.trim()) params.set('topic', topic.trim())
      const res = await fetch(`/digest/fetch?${params}`, {
        method: 'POST',
        headers: authHeaders(),
      })
      const done = await stream(res, (msg) => {
        if (msg.progress) {
          const { done: n, total, url: at } = msg.progress
          pct = total ? Math.round((n / total) * 100) : null
          status = `fetching ${n}/${total} — ${at}`
        } else if (msg.pages !== undefined) {
          return msg
        }
      })
      if (!done) throw new Error('the fetch ended without a result')
      pages = done.pages
      dropped = done.dropped
      assemble()
      const kept = pages.filter((p) => p.kept).length
      status = `fetched ${pages.length} page(s), ${kept} selected — ${text.length} chars`
    } catch (err) {
      error = err.message
    } finally {
      busy = false
      pct = null
    }
  }

  function togglePage(i) {
    pages[i].kept = !pages[i].kept
    assemble()
  }

  async function onFile(e) {
    const file = e.target.files?.[0]
    if (!file) return
    error = null
    busy = true
    thinking = true
    pct = null
    status = `reading ${file.name}…`
    try {
      const buf = await file.arrayBuffer()
      const res = await fetch(`/digest/extract?name=${encodeURIComponent(file.name)}`, {
        method: 'POST',
        headers: authHeaders(),
        body: buf,
      })
      // No per-page progress to report: conversion is one call, so the spinning
      // overlay carries it and `pct` stays null. A crawl still reports pages —
      // see the handler above.
      const done = await stream(res, (msg) => {
        if (msg.text !== undefined) return msg
      })
      if (!done) throw new Error('extraction ended without a result')
      text = done.text
      // An uploaded file replaces whatever a crawl had put in the box.
      pages = []
      dropped = []
      proposal = null
      status = `extracted ${done.chars} chars from ${file.name}`
    } catch (err) {
      error = err.message
    } finally {
      busy = false
      thinking = false
      pct = null
      e.target.value = '' // allow re-selecting the same file
    }
  }

  async function preview() {
    error = null
    busy = true
    thinking = true
    status = 'summoning the LLM…'
    proposal = null
    try {
      proposal = await rpc('digest.run', {
        plane,
        text,
        chat,
        embed,
        no_embed: noEmbed,
        link,
        mode,
      })
      const r = proposal.report
      const linked = r.linked ? `, ${r.linked} linked to existing` : ''
      status = `proposal: ${r.entities} entities, ${r.relations} relations${linked}`
    } catch (err) {
      error = err.message
    } finally {
      busy = false
      thinking = false
    }
  }

  async function write() {
    if (!proposal) return
    error = null
    busy = true
    status = 'writing to graph…'
    try {
      const r = await rpc('digest.write', {
        plane,
        nodes: proposal.nodes,
        edges: proposal.edges,
      })
      status = `wrote ${r.nodes_written} nodes, ${r.edges_written} edges into "${plane}"`
      proposal = null
    } catch (err) {
      error = err.message
    } finally {
      busy = false
    }
  }
</script>

<div class="controls">
  <label>
    Chat
    <select bind:value={chat}>
      {#each PROVIDERS as p (p)}<option value={p}>{p}</option>{/each}
    </select>
  </label>
  <label>
    Embed
    <select bind:value={embed}>
      {#each PROVIDERS as p (p)}<option value={p}>{p}</option>{/each}
    </select>
  </label>
  <label title={MODES.find((m) => m[0] === mode)?.[1]}>
    Mode
    <select bind:value={mode} onchange={() => savePref('digestMode', mode)}>
      {#each MODES as [name, what] (name)}<option value={name} title={what}>{name}</option>{/each}
    </select>
  </label>
  <label class="check"><input type="checkbox" bind:checked={noEmbed} /> no embeddings</label>
  <label class="check" title="Retrieve similar entities already in the plane and let the LLM reuse their keys / add edges to them, instead of creating duplicates">
    <input type="checkbox" bind:checked={link} /> link to existing nodes
  </label>
  <button class="new-plane-btn ml-auto" onclick={() => (newPlaneOpen = true)} title="Create a new plane">New Plane</button>
</div>

<!-- Row two: where the document comes from. Kept apart from the parse
     settings above because these are two different questions — what to read,
     and how to read it. -->
<div class="sources">
  <label
    class="file-btn"
    title="Markdown and plain text, or Word, PowerPoint, Excel, OpenDocument, RTF, EPUB, CSV and PDF — converted to Markdown"
  >
    Local file: upload
    <input type="file" accept=".md,.markdown,.txt,.text,.doc,.docx,.odt,.rtf,.epub,.pdf,.ppt,.pptx,.xls,.xlsx,.ods,.odp,.csv" onchange={onFile} />
  </label>
  <span class="ext">.md .txt .pdf .docx .pptx .xlsx .csv &amp; more</span>
  <span class="or">or from URL</span>
  <input
    class="url"
    type="url"
    bind:value={url}
    placeholder="https://example.com/article"
    onkeydown={(e) => e.key === 'Enter' && !busy && fetchUrl()}
  />
  <label title="How far to follow the page's links. 0 reads only the page you named; each further hop costs requests and tokens.">
    follow links
    <select bind:value={depth} onchange={() => savePref('digestDepth', String(depth))}>
      {#each [0, 1, 2, 3] as d (d)}<option value={d}>{d}</option>{/each}
    </select>
  </label>
  <input
    class="topic"
    type="text"
    bind:value={topic}
    placeholder="topic (optional)"
    title="Sharpens which links are worth following, beyond what the page itself is about. Left empty, the page's own subject is the target."
  />
  <button onclick={fetchUrl} disabled={busy || !url.trim()}>Fetch</button>
</div>

<CreatePlane bind:open={newPlaneOpen} onCreated={onPlaneCreated} />

{#if mode === 'super'}
  <p class="cost-warn" role="alert">
    <span class="cost-mark" aria-hidden="true">⚠</span>
    <span>
      <strong>super</strong> re-reads every entity against all the passages mentioning it: the most
      accurate digest, and <strong>~15× the input token usage</strong> — one extra request per
      entity that has something new to read.
    </span>
  </p>
{/if}

{#if error}<p class="error">{error}</p>{/if}
{#if status}<p class="status">{status}</p>{/if}
{#if busy && pct !== null}
  <div class="progress" role="progressbar" aria-valuenow={pct} aria-valuemin="0" aria-valuemax="100">
    <div class="bar" style="width:{pct}%"></div>
  </div>
{/if}

{#if pages.length}
  <section class="pages">
    <h3>
      Fetched {pages.length} page(s) — {pages.filter((p) => p.kept).length} selected
      <span class="hint">most relevant first; untick anything not worth the tokens</span>
    </h3>
    <ul>
      {#each pages as p, i (p.url)}
        <li class:off={!p.kept}>
          <label>
            <input type="checkbox" checked={p.kept} onchange={() => togglePage(i)} />
            <span class="score" title="Relevance to the target, 0–1">{p.score.toFixed(2)}</span>
            <span class="ptitle">{p.title || p.url}</span>
            {#if p.depth === 0}<span class="badge">the page you named</span>{/if}
            <span class="chars">{p.chars.toLocaleString()} chars</span>
          </label>
          <a class="purl" href={p.url} target="_blank" rel="noreferrer noopener">{p.url}</a>
        </li>
      {/each}
    </ul>
    {#if dropped.length}
      <details class="dropped">
        <summary>{dropped.length} not kept</summary>
        <ul>
          {#each dropped as d (d.url)}
            <li><span class="purl">{d.url}</span> — {d.reason}</li>
          {/each}
        </ul>
      </details>
    {/if}
  </section>
{/if}

<textarea
  class="doc"
  bind:value={text}
  rows="12"
  placeholder="Upload a document — Markdown, text, PDF, Word, PowerPoint, Excel, OpenDocument, RTF, EPUB or CSV — paste a URL above, or paste text here…"
></textarea>

<div class="actions">
  <button onclick={preview} disabled={busy || !text.trim()}>Preview (LLM)</button>
  <button class="primary" onclick={write} disabled={busy || !proposal}>Write to graph</button>
</div>

{#if proposal}
  <section class="proposal">
    <div class="report">
      {proposal.report.chunks} chunks · {proposal.report.entities} new entities{#if proposal.report.linked}
        ({proposal.report.linked} linked to existing){/if} ·
      {proposal.report.relations} relations ({proposal.report.dropped_relations} dropped) · tokens
      {proposal.report.input_tokens}/{proposal.report.output_tokens} chat,
      {proposal.report.embed_tokens} embed
    </div>
    <div class="cols">
      <div>
        <h3>Entities ({proposal.nodes.length})</h3>
        <ul>
          {#each proposal.nodes as n (n.key)}
            <li><span class="lbl">{n.label}</span> {n.key}</li>
          {/each}
        </ul>
      </div>
      <div>
        <h3>Relations ({proposal.edges.length})</h3>
        <ul>
          {#each proposal.edges as e, i (i)}
            <li>{e.src} <span class="ty">{e.type}</span> {e.dst}</li>
          {/each}
        </ul>
      </div>
    </div>
  </section>
{/if}

<p class="hint">
  Preview runs the LLM once — provider API keys come from the server's environment
  (OPENAI_API_KEY / DEEPSEEK_API_KEY / DASHSCOPE_API_KEY). Write commits the previewed proposal
  with no second LLM call.
</p>

{#if thinking}
  <div class="thinking-overlay">
    <div class="thinking-box">
      <svg
        class="portal"
        viewBox="0 0 64 64"
        fill="none"
        stroke="#d9a441"
        stroke-width="2"
        stroke-linejoin="round"
        aria-hidden="true"
      >
        <g class="cw">
          <circle cx="32" cy="32" r="30" />
          <circle cx="32" cy="32" r="25.5" stroke-width="3" stroke-dasharray="1.2 3" />
        </g>
        <g class="ccw">
          <rect x="16" y="16" width="32" height="32" />
          <rect x="16" y="16" width="32" height="32" transform="rotate(45 32 32)" />
          <circle cx="32" cy="32" r="11" />
        </g>
        <circle class="core" cx="32" cy="32" r="3.5" fill="#d9a441" stroke="none" />
      </svg>
      <p>{status}</p>
    </div>
  </div>
{/if}
