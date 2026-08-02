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
  let proposal = $state(null) // { report, nodes, edges }
  let status = $state('')
  let error = $state(null)
  let busy = $state(false)
  let pct = $state(null) // extraction progress 0..100, null = no/indeterminate
  let thinking = $state(false) // waiting on the (slow, indeterminate) LLM call

  async function onFile(e) {
    const file = e.target.files?.[0]
    if (!file) return
    error = null
    busy = true
    pct = null
    status = `extracting ${file.name}…`
    try {
      const buf = await file.arrayBuffer()
      const res = await fetch(`/digest/extract?name=${encodeURIComponent(file.name)}`, {
        method: 'POST',
        headers: authHeaders(),
        body: buf,
      })
      // A non-2xx here is a pre-stream failure (e.g. a 413 when the upload is
      // too large) with a non-streamed body — read it defensively.
      if (!res.ok) {
        const raw = await res.text()
        let msg = raw
        try {
          msg = JSON.parse(raw).error || raw
        } catch {
          /* keep raw */
        }
        throw new Error(msg || `extraction failed (${res.status})`)
      }

      // Success is a stream of newline-delimited JSON: progress lines then a
      // final {chars,text} (or {error}). Parse it line by line to drive the bar.
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
          if (msg.progress) {
            const { page, total } = msg.progress
            pct = total ? Math.round((page / total) * 100) : null
            status = `extracting ${file.name}… page ${page}/${total}`
          } else if (msg.error) {
            throw new Error(msg.error)
          } else if (msg.text !== undefined) {
            done = msg
          }
        }
      }
      if (!done) throw new Error('extraction ended without a result')
      text = done.text
      proposal = null
      status = `extracted ${done.chars} chars from ${file.name}`
    } catch (err) {
      error = err.message
    } finally {
      busy = false
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
  <label class="file-btn">
    Upload file
    <input type="file" accept=".md,.markdown,.txt,.pdf,.docx" onchange={onFile} />
  </label>
  <button class="new-plane-btn ml-auto" onclick={() => (newPlaneOpen = true)} title="Create a new plane">New Plane</button>
</div>

<CreatePlane bind:open={newPlaneOpen} onCreated={onPlaneCreated} />

{#if mode === 'super'}
  <p class="hint">
    <strong>super</strong> re-reads every entity against all the passages mentioning it — the most
    accurate digest, and <strong>~15× the input token usage</strong>: one extra request per entity
    that has something new to read.
  </p>
{/if}

{#if error}<p class="error">{error}</p>{/if}
{#if status}<p class="status">{status}</p>{/if}
{#if busy && pct !== null}
  <div class="progress" role="progressbar" aria-valuenow={pct} aria-valuemin="0" aria-valuemax="100">
    <div class="bar" style="width:{pct}%"></div>
  </div>
{/if}

<textarea
  class="doc"
  bind:value={text}
  rows="12"
  placeholder="Upload a markdown / txt / pdf / docx file, or paste text here…"
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
