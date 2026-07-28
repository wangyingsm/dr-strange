<script>
  import { onMount, onDestroy } from 'svelte'
  import { rpc } from './rpc.js'
  import { Plot } from './plot.js'

  let container // canvas div (bind:this)
  let plot = null

  let planes = $state([])
  let plane = $state('startup')
  let labels = $state([]) // catalog label names for the filter
  let labelFilter = $state('') // '' = all labels
  let selected = $state(null) // { kind: 'node'|'edge', data }
  let legend = $state([])
  let status = $state('')
  let error = $state(null)
  let vectorView = $state(null) // { k, values } — floats popup, null = closed

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

  async function loadPlanes() {
    planes = await rpc('plane.list')
    if (!planes.some((p) => p.name === plane)) plane = planes[0]?.name ?? 'startup'
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

  async function changePlane() {
    labelFilter = ''
    await loadCatalog()
    await seed()
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
    })
    await loadPlanes()
    await loadCatalog()
    await seed()
  })

  onDestroy(() => plot?.destroy())
</script>

<div class="controls">
  <label>
    Plane
    <select bind:value={plane} onchange={changePlane}>
      {#each planes as p (p.id)}
        <option value={p.name}>{p.name}</option>
      {/each}
    </select>
  </label>
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
</div>

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

<p class="hint">Click a node to expand its neighbourhood and inspect it. Click empty space to deselect.</p>
