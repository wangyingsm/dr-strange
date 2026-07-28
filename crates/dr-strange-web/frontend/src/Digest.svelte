<script>
  import { rpc } from './rpc.js'

  const PROVIDERS = ['openai', 'deepseek', 'qwen', 'ollama']

  let plane = $state('startup')
  let text = $state('')
  let chat = $state('openai')
  let embed = $state('openai')
  let noEmbed = $state(false)
  let proposal = $state(null) // { report, nodes, edges }
  let status = $state('')
  let error = $state(null)
  let busy = $state(false)

  async function onFile(e) {
    const file = e.target.files?.[0]
    if (!file) return
    error = null
    busy = true
    status = `extracting ${file.name}…`
    try {
      const buf = await file.arrayBuffer()
      const res = await fetch(`/digest/extract?name=${encodeURIComponent(file.name)}`, {
        method: 'POST',
        body: buf,
      })
      const data = await res.json()
      if (!res.ok) throw new Error(data.error || 'extraction failed')
      text = data.text
      proposal = null
      status = `extracted ${data.chars} chars from ${file.name}`
    } catch (err) {
      error = err.message
    } finally {
      busy = false
      e.target.value = '' // allow re-selecting the same file
    }
  }

  async function preview() {
    error = null
    busy = true
    status = 'running digest (calling the LLM)…'
    proposal = null
    try {
      proposal = await rpc('digest.run', {
        plane,
        text,
        chat,
        embed,
        no_embed: noEmbed,
      })
      const r = proposal.report
      status = `proposal: ${r.entities} entities, ${r.relations} relations`
    } catch (err) {
      error = err.message
    } finally {
      busy = false
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
  <label>Plane <input class="txt" bind:value={plane} /></label>
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
  <label class="check"><input type="checkbox" bind:checked={noEmbed} /> no embeddings</label>
  <label class="file-btn">
    Upload file
    <input type="file" accept=".md,.markdown,.txt,.pdf,.docx" onchange={onFile} />
  </label>
</div>

{#if error}<p class="error">{error}</p>{/if}
{#if status}<p class="status">{status}</p>{/if}

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
      {proposal.report.chunks} chunks · {proposal.report.entities} entities ·
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
