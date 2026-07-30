<script>
  import { onMount, onDestroy } from 'svelte'
  import { rpc, authHeaders } from './rpc.js'
  import { Plot } from './plot.js'
  import CreatePlane from './CreatePlane.svelte'

  // Providers with an embedding endpoint (deepseek is chat-only, so excluded) —
  // used to embed a text `SEARCH … NEAR "…"`.
  const EMBED_PROVIDERS = ['openai', 'qwen', 'ollama']

  // `plane` is the app-wide current plane (App owns it); `focus` is a
  // { id, nonce } signal from the header search — center that node.
  // `onPlaneCreated` bubbles a new plane up to App (refresh picker + switch).
  let { plane, focus, onPlaneCreated = () => {} } = $props()

  let newPlaneOpen = $state(false) // new-plane popup open?

  let container // canvas div (bind:this)
  let plot = null
  let started = false // plot created + first seed done
  let seededPlane = null // last plane we seeded (avoids re-seeding on no-op)
  let appliedFocus = -1 // last focus nonce we centered (idempotent)

  let labels = $state([]) // catalog label names for the filter
  let labelFilter = $state('') // '' = all labels
  let cypher = $state('') // query-language text; '' = use the label seed
  let embedProvider = $state('openai') // provider for a text SEARCH … NEAR "…"
  let selected = $state(null) // { kind: 'node'|'edge', data }
  let legend = $state([])
  let status = $state('')
  let error = $state(null)
  let vectorView = $state(null) // { k, values } — floats popup, null = closed

  // Inspector mutation state (mutation UI): edit properties + delete.
  let editing = $state(false)
  let draft = $state([]) // editable [{ key, value }] rows of scalar props
  let newKey = $state('')
  let newValue = $state('')
  let saveError = $state(null)
  let draftLabels = $state('') // node: comma-separated labels being edited
  let draftType = $state('') // edge: type being edited

  // Inspector delete (type-to-confirm popup).
  let confirmingDelete = $state(false)
  let deleteInput = $state('')
  let deleteError = $state(null)

  // Create-node / create-edge state.
  let creating = $state(null) // null | 'node' | 'edge'
  let createError = $state(null)
  let nKey = $state('')
  let nLabels = $state('')
  let eSrc = $state('')
  let eDst = $state('')
  let eType = $state('')

  // Flatten a properties object (values are raw, `{ $desc, $value }`, or an
  // embedding `{ $vector: [...] }`), then sink underscore-prefixed
  // provenance/internal props to the bottom — each group keeps its order.
  function propEntries(props) {
    const entries = Object.entries(props ?? {}).map(([k, raw]) => {
      let v = raw
      let desc = null
      if (v && typeof v === 'object' && '$value' in v) {
        desc = v.$desc
        v = v.$value
      }
      if (v && typeof v === 'object' && Array.isArray(v.$vector)) {
        v = v.$vector // unwrap embeddings so they collapse to a button
      }
      return { k, v, desc }
    })
    const inner = entries.filter((e) => e.k.startsWith('_'))
    const outer = entries.filter((e) => !e.k.startsWith('_'))
    return [...outer, ...inner]
  }

  // A numeric array = an embedding vector; render a button, not 128+ floats.
  const isVector = (v) => Array.isArray(v) && v.length > 0 && v.every((x) => typeof x === 'number')

  // Pretty grid: fixed-width columns of 6 values, index-addressable via rows.
  function formatVector(v) {
    const cols = 6
    const rows = []
    for (let i = 0; i < v.length; i += cols) {
      rows.push(v.slice(i, i + cols).map((x) => x.toFixed(5).padStart(10)).join(' '))
    }
    return rows.join('\n')
  }

  async function loadCatalog() {
    try {
      const cat = await rpc('plane.catalog', { plane })
      labels = Object.keys(cat.labels ?? {})
    } catch {
      labels = []
    }
  }

  async function seed() {
    error = null
    try {
      plot.clear()
      const params = { plane }
      if (labelFilter) params.label = labelFilter
      const sg = await rpc('graph.seed', params)
      plot.addSubgraph(sg)
      legend = plot.legendEntries()
      selected = null
      status = `${sg.nodes.length} nodes · ${sg.edges.length} edges${
        sg.truncated ? ` (of ${sg.total}, capped)` : ''
      }`
    } catch (e) {
      error = e.message
    }
  }

  // Run a query-language string against the current plane and render its
  // result (nodes + induced edges) as a fresh subgraph. Empty query → fall
  // back to the plain label seed. Hits the web-only POST /cypher endpoint
  // (not an RPC method), so it uses a raw fetch with the bearer token.
  async function runCypher() {
    if (!cypher.trim()) {
      await seed()
      return
    }
    error = null
    try {
      const url = `/cypher?plane=${encodeURIComponent(plane)}&embed=${encodeURIComponent(embedProvider)}`
      const res = await fetch(url, {
        method: 'POST',
        headers: authHeaders({ 'content-type': 'text/plain' }),
        body: cypher,
      })
      if (!res.ok) throw new Error((await res.text()) || `query failed (${res.status})`)
      const sg = await res.json()
      plot.clear()
      plot.addSubgraph(sg)
      legend = plot.legendEntries()
      selected = null
      status = `${sg.count} nodes · ${sg.edges.length} edges`
    } catch (e) {
      error = e.message
    }
  }

  async function expand(id) {
    try {
      const sg = await rpc('graph.expand', { plane, id, direction: 'both' })
      plot.addSubgraph(sg, id)
      legend = plot.legendEntries()
      status = `expanded +${sg.nodes.length} nodes${
        sg.truncated ? ` (${sg.total - sg.nodes.length} more not shown)` : ''
      }`
    } catch (e) {
      error = e.message
    }
  }

  // ---- inspector mutations ------------------------------------------------

  // Only plain scalar, non-provenance props are editable — vectors (embeddings)
  // and underscore-prefixed provenance stay read-only.
  function editableEntries(props) {
    return propEntries(props).filter((e) => !e.k.startsWith('_') && !isVector(e.v))
  }

  // Parse an input the way the backend will: valid JSON (42, true, "x") keeps
  // its type; anything else is a plain string.
  function parseValue(s) {
    try {
      return JSON.parse(s)
    } catch {
      return s
    }
  }

  function startEdit() {
    saveError = null
    draft = editableEntries(selected.data.properties).map((e) => ({
      key: e.k,
      value: typeof e.v === 'string' ? e.v : JSON.stringify(e.v),
    }))
    newKey = ''
    newValue = ''
    draftLabels = selected.kind === 'node' ? (selected.data.labels ?? []).join(', ') : ''
    draftType = selected.kind === 'edge' ? (selected.data.type ?? '') : ''
    editing = true
  }

  function cancelEdit() {
    editing = false
    draft = []
    saveError = null
  }

  function removeDraftRow(i) {
    draft = draft.filter((_, j) => j !== i)
  }

  async function saveEdit() {
    saveError = null
    const rows = [...draft]
    if (newKey.trim()) rows.push({ key: newKey.trim(), value: newValue })

    // `set` = every kept row; `unset` = editable keys that were removed.
    const set = {}
    for (const r of rows) if (r.key.trim()) set[r.key.trim()] = parseValue(r.value)
    const kept = new Set(rows.map((r) => r.key.trim()))
    const unset = editableEntries(selected.data.properties)
      .map((e) => e.k)
      .filter((k) => !kept.has(k))

    try {
      let updated
      if (selected.kind === 'node') {
        const labels = draftLabels
          .split(',')
          .map((s) => s.trim())
          .filter(Boolean)
        updated = await rpc('node.update', { plane, id: selected.data.id, set, unset, labels })
      } else {
        const params = { plane, edge: selected.data.id, set, unset }
        if (draftType.trim()) params.type = draftType.trim()
        updated = await rpc('edge.update', params)
      }
      selected = { kind: selected.kind, data: updated }
      editing = false
      status = `updated ${selected.kind} ${updated.id}`
    } catch (e) {
      saveError = e.message
    }
  }

  // The token the user must type to confirm a delete: a node's external key
  // (or its id when keyless), an edge's id.
  function deleteToken() {
    if (!selected) return ''
    return selected.kind === 'node'
      ? (selected.data.external_key ?? String(selected.data.id))
      : String(selected.data.id)
  }

  function askDelete() {
    deleteInput = ''
    deleteError = null
    confirmingDelete = true
  }
  function cancelDelete() {
    confirmingDelete = false
  }

  async function confirmDelete() {
    if (!selected || deleteInput.trim() !== deleteToken()) return // type-to-confirm
    const kind = selected.kind
    const token = deleteToken()
    deleteError = null
    try {
      if (kind === 'node') await rpc('node.delete', { plane, id: selected.data.id })
      else await rpc('edge.delete', { plane, edge: selected.data.id })
      confirmingDelete = false
      selected = null
      editing = false
      await seed() // reload the canvas without the deleted element
      status = `deleted ${kind} ${token}`
    } catch (e) {
      deleteError = e.message
    }
  }

  function resetCreate() {
    creating = null
    createError = null
    nKey = ''
    nLabels = ''
    eSrc = ''
    eDst = ''
    eType = ''
  }

  function openCreate(kind) {
    resetCreate()
    creating = kind
  }

  function autofocus(el) {
    el.focus()
  }

  function onKeydown(e) {
    if (e.key !== 'Escape') return
    if (creating) resetCreate()
    else if (confirmingDelete) cancelDelete()
  }

  async function createNode() {
    createError = null
    const labels = nLabels
      .split(',')
      .map((s) => s.trim())
      .filter(Boolean)
    const params = { plane, labels }
    if (nKey.trim()) params.key = nKey.trim()
    try {
      const node = await rpc('node.create', params)
      resetCreate()
      await focusNode(node.id) // center + select the new node (then Edit for props)
    } catch (e) {
      createError = e.message
    }
  }

  // A numeric string is a node id; anything else is an external key (matches
  // the backend's NodeRef).
  const nodeRef = (s) => (/^\d+$/.test(s.trim()) ? Number(s.trim()) : s.trim())

  async function createEdge() {
    createError = null
    if (!eType.trim()) {
      createError = 'edge type is required'
      return
    }
    try {
      const edge = await rpc('edge.create', {
        plane,
        src: nodeRef(eSrc),
        dst: nodeRef(eDst),
        type: eType.trim(),
      })
      resetCreate()
      // If both endpoints are already on the canvas (the common case for a
      // drag-connect), just drop the edge in — no relayout, graph stays put.
      if (plot.hasNode(edge.src) && plot.hasNode(edge.dst)) {
        plot.addEdgeInPlace(edge)
        plot.selectEdge(edge.id)
        selected = { kind: 'edge', data: edge }
        status = `created edge ${edge.id}`
      } else {
        await focusEdge(edge) // an endpoint isn't shown — center on the new edge
      }
    } catch (e) {
      createError = e.message
    }
  }

  // Leaving a selection (or switching to a new one) drops any in-progress edit.
  $effect(() => {
    selected // track
    editing = false
  })

  // Center a specific node (from header search): show it plus its 1-hop
  // neighborhood and select it.
  async function focusNode(id) {
    error = null
    try {
      const node = await rpc('node.get', { plane, id })
      if (!node) {
        status = `node ${id} not found`
        return
      }
      plot.clear()
      plot.addSubgraph({ nodes: [node], edges: [] })
      const sg = await rpc('graph.expand', { plane, id, direction: 'both' })
      plot.addSubgraph(sg, id)
      plot.selectNode(id) // keep the focused node lit
      legend = plot.legendEntries()
      selected = { kind: 'node', data: node }
      status = `focused ${node.external_key ?? `#${id}`} · +${sg.nodes.length} neighbors`
    } catch (e) {
      error = e.message
    }
  }

  // Center an edge (from header search): show both endpoints + their
  // neighborhoods and select the edge itself.
  async function focusEdge(edge) {
    error = null
    try {
      const [src, dst] = await Promise.all([
        rpc('node.get', { plane, id: edge.src }),
        rpc('node.get', { plane, id: edge.dst }),
      ])
      plot.clear()
      plot.addSubgraph({ nodes: [src, dst].filter(Boolean), edges: [edge] })
      for (const id of [edge.src, edge.dst]) {
        plot.addSubgraph(await rpc('graph.expand', { plane, id, direction: 'both' }), id)
      }
      plot.selectEdge(edge.id) // keep the found edge lit
      legend = plot.legendEntries()
      selected = { kind: 'edge', data: edge }
      status = `focused ${edge.type}: ${src?.external_key ?? `#${edge.src}`} → ${dst?.external_key ?? `#${edge.dst}`}`
    } catch (e) {
      error = e.message
    }
  }

  async function applyFocus() {
    if (focus.kind === 'edge') await focusEdge(focus.edge)
    else await focusNode(focus.id)
  }

  // Re-seed when the current plane changes (order-independent: whoever runs
  // first — onMount or the effect below — seeds once, the other no-ops).
  async function reseed() {
    if (!started || plane === seededPlane) return
    seededPlane = plane
    labelFilter = ''
    await loadCatalog()
    await seed()
  }

  // Apply a pending focus signal once (idempotent by nonce).
  async function maybeFocus() {
    if (!started || !focus || focus.nonce === appliedFocus) return
    appliedFocus = focus.nonce
    await applyFocus()
  }

  onMount(async () => {
    plot = new Plot(container, {
      onSelectNode: (_node, record) => {
        selected = { kind: 'node', data: record }
        expand(record.id)
      },
      onSelectEdge: (_edge, record) => {
        selected = { kind: 'edge', data: record }
      },
      onClearSelection: () => {
        selected = null
      },
      // Left-drag from one node to another: open the New Edge popup with the
      // endpoints prefilled (the user just supplies the edge type).
      onConnect: (src, dst) => {
        resetCreate()
        eSrc = src.external_key ?? String(src.id)
        eDst = dst.external_key ?? String(dst.id)
        creating = 'edge'
      },
    })
    started = true
    if (focus) {
      // Arrived here from a search hit — go straight to that node/edge's
      // neighborhood instead of seeding (then discarding) the whole plane.
      seededPlane = plane
      appliedFocus = focus.nonce
      await applyFocus()
    } else {
      await reseed()
    }
  })

  // React to plane switches and search focuses after mount.
  $effect(() => {
    plane // track
    reseed()
  })
  $effect(() => {
    focus // track
    maybeFocus()
  })

  onDestroy(() => plot?.destroy())
</script>

<svelte:window onkeydown={onKeydown} />

<div class="controls">
  <label>
    Label
    <select bind:value={labelFilter} onchange={seed}>
      <option value="">all</option>
      {#each labels as l (l)}
        <option value={l}>{l}</option>
      {/each}
    </select>
  </label>
  <button onclick={seed}>Reload</button>
  <span class="status">{status}</span>
  <button class="new-node-btn" onclick={() => openCreate('node')} title="Create a node">New Node</button>
  <button class="new-edge-btn" onclick={() => openCreate('edge')} title="Create an edge">New Edge</button>
  <button class="new-plane-btn" onclick={() => (newPlaneOpen = true)} title="Create a new plane">New Plane</button>
</div>

<div class="query-bar">
  <input
    type="text"
    class="cypher"
    placeholder={'MATCH (n:Label) WHERE n.p > 1 RETURN n LIMIT 50   ·   SEARCH (n:Label) ON embedding NEAR "some text" TOPK 10 RETURN n'}
    bind:value={cypher}
    onkeydown={(e) => e.key === 'Enter' && runCypher()}
  />
  <label class="embed-pick" title={'Embedding provider for a text SEARCH … NEAR "…" (must match how the plane was embedded)'}>
    embed
    <select bind:value={embedProvider}>
      {#each EMBED_PROVIDERS as pv (pv)}<option value={pv}>{pv}</option>{/each}
    </select>
  </label>
  <button class="run-btn" onclick={runCypher} title="Run this query and plot the result">Run</button>
</div>

<CreatePlane bind:open={newPlaneOpen} onCreated={onPlaneCreated} />

{#if creating === 'node'}
  <div class="dlg-backdrop">
    <div class="dlg" role="dialog" aria-modal="true" aria-label="Create a node">
      <header>
        New node
        <button class="close" onclick={resetCreate} aria-label="Close">×</button>
      </header>
      <div class="dlg-body">
        <input placeholder="key (optional)" bind:value={nKey} use:autofocus onkeydown={(e) => e.key === 'Enter' && createNode()} />
        <input placeholder="labels (comma-separated)" bind:value={nLabels} onkeydown={(e) => e.key === 'Enter' && createNode()} />
        {#if createError}<p class="dlg-error">{createError}</p>{/if}
        <div class="dlg-actions">
          <button class="ghost" onclick={resetCreate}>Cancel</button>
          <button class="primary" onclick={createNode}>Create node</button>
        </div>
      </div>
    </div>
  </div>
{:else if creating === 'edge'}
  <div class="dlg-backdrop">
    <div class="dlg" role="dialog" aria-modal="true" aria-label="Create an edge">
      <header>
        New edge
        <button class="close" onclick={resetCreate} aria-label="Close">×</button>
      </header>
      <div class="dlg-body">
        <input placeholder="src (key or id)" bind:value={eSrc} use:autofocus onkeydown={(e) => e.key === 'Enter' && createEdge()} />
        <input placeholder="dst (key or id)" bind:value={eDst} onkeydown={(e) => e.key === 'Enter' && createEdge()} />
        <input placeholder="type" bind:value={eType} onkeydown={(e) => e.key === 'Enter' && createEdge()} />
        {#if createError}<p class="dlg-error">{createError}</p>{/if}
        <div class="dlg-actions">
          <button class="ghost" onclick={resetCreate}>Cancel</button>
          <button class="primary" onclick={createEdge}>Create edge</button>
        </div>
      </div>
    </div>
  </div>
{/if}

{#if error}<p class="error">{error}</p>{/if}

<div class="canvas-wrap">
  <div class="canvas" bind:this={container}></div>

  {#if legend.length}
    <div class="legend">
      {#each legend as e (e.label)}
        <span class="swatch" style="--c:{e.color}">{e.label || '(no label)'}</span>
      {/each}
    </div>
  {/if}

  {#if selected}
    <aside class="inspector">
      {#if selected.kind === 'node'}
        <h3>Node {selected.data.id}</h3>
        {#if selected.data.external_key}<p class="key">@{selected.data.external_key}</p>{/if}
        <p class="sub">{selected.data.labels?.join(', ') || '(no labels)'}</p>
      {:else}
        <h3>Edge {selected.data.id}</h3>
        <p class="sub">{selected.data.type} · {selected.data.src} → {selected.data.dst}</p>
      {/if}
      {#if !editing}
        <dl>
          {#each propEntries(selected.data.properties) as pe (pe.k)}
            <dt title={pe.desc ?? ''}>{pe.k}{#if pe.desc}<span class="badge" title={pe.desc}>ℹ</span>{/if}</dt>
            <dd>
              {#if isVector(pe.v)}
                <button class="vec-btn" onclick={() => (vectorView = { k: pe.k, values: pe.v })}>
                  show vector ({pe.v.length} dims)
                </button>
              {:else}
                {typeof pe.v === 'string' ? pe.v : JSON.stringify(pe.v)}
              {/if}
            </dd>
          {/each}
        </dl>
        <div class="inspector-actions">
          <button onclick={startEdit}>Edit</button>
          <button class="danger" onclick={askDelete}>Delete</button>
        </div>
      {:else}
        <div class="edit-field">
          {#if selected.kind === 'node'}
            <span>Labels</span>
            <input bind:value={draftLabels} placeholder="comma-separated" />
          {:else}
            <span>Type</span>
            <input bind:value={draftType} placeholder="edge type" />
          {/if}
        </div>
        <div class="edit-props">
          {#each draft as row, i (i)}
            <div class="edit-row">
              <input class="pk" value={row.key} readonly />
              <input class="pv" bind:value={row.value} />
              <button class="rm" title="remove property" onclick={() => removeDraftRow(i)}>×</button>
            </div>
          {/each}
          <div class="edit-row new">
            <input class="pk" placeholder="new key" bind:value={newKey} />
            <input class="pv" placeholder="value" bind:value={newValue} />
          </div>
        </div>
        {#if saveError}<p class="error">{saveError}</p>{/if}
        <p class="edit-note">Values parse as JSON (<code>42</code>, <code>true</code>, <code>"text"</code>); vectors &amp; _provenance are read-only.</p>
        <div class="inspector-actions">
          <button class="primary" onclick={saveEdit}>Save</button>
          <button onclick={cancelEdit}>Cancel</button>
        </div>
      {/if}
    </aside>
  {/if}

  {#if vectorView}
    <div class="modal-backdrop">
      <div class="modal">
        <header>
          <span>{vectorView.k} · {vectorView.values.length} dims</span>
          <button class="close" onclick={() => (vectorView = null)}>×</button>
        </header>
        <pre class="floats">{formatVector(vectorView.values)}</pre>
      </div>
    </div>
  {/if}
</div>

{#if confirmingDelete && selected}
  <div class="dlg-backdrop">
    <div class="dlg" role="dialog" aria-modal="true" aria-label="Delete {selected.kind}">
      <header>
        Delete {selected.kind}
        <button class="close" onclick={cancelDelete} aria-label="Close">×</button>
      </header>
      <div class="dlg-body">
        <p class="dlg-warn">
          This permanently deletes {selected.kind} <b>{deleteToken()}</b>{#if selected.kind === 'node'} and its incident edges{/if}. This cannot be undone.
        </p>
        <label class="dlg-confirm">
          Type <code>{deleteToken()}</code> to confirm
          <input
            bind:value={deleteInput}
            use:autofocus
            onkeydown={(e) => e.key === 'Enter' && confirmDelete()}
          />
        </label>
        {#if deleteError}<p class="dlg-error">{deleteError}</p>{/if}
        <div class="dlg-actions">
          <button class="ghost" onclick={cancelDelete}>Cancel</button>
          <button class="danger" onclick={confirmDelete} disabled={deleteInput.trim() !== deleteToken()}>
            Delete
          </button>
        </div>
      </div>
    </div>
  </div>
{/if}

<p class="hint">Click a node to expand and inspect it; click empty space to deselect. Drag from one node onto another to connect them. Hold the right mouse button to pan.</p>
