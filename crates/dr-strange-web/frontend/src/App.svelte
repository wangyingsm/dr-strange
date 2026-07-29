<script>
  import { onMount } from 'svelte'
  import { rpc } from './rpc.js'
  import Dashboard from './Dashboard.svelte'
  import Explore from './Explore.svelte'
  import Digest from './Digest.svelte'

  // Providers with an embedding endpoint (deepseek is chat-only, so excluded).
  const EMBED_PROVIDERS = ['openai', 'qwen', 'ollama']

  let view = $state('dashboard')
  let planes = $state([]) // [{ id, name, ... }]
  let plane = $state('startup') // app-wide current plane (browse + search)
  let q = $state('') // search box
  let semantic = $state(false) // text substring vs embedding-similarity search
  let embedProvider = $state('openai')
  let results = $state(null) // { nodes, edges, mode, note, ... } | { error }
  let focus = $state(null) // { id, nonce } → Explore centers this node

  let nonce = 0
  let timer

  async function loadPlanes() {
    planes = await rpc('plane.list')
  }

  // The Dashboard owns plane creation (a "+" card + popup); when it makes one,
  // refresh the header picker and switch to the new plane.
  function onPlaneCreated(name) {
    loadPlanes()
    plane = name
  }

  onMount(async () => {
    try {
      await loadPlanes()
      if (!planes.some((p) => p.name === plane)) plane = planes[0]?.name ?? 'startup'
    } catch {
      // Dashboard still works; the header just shows the default plane.
    }
  })

  // Debounced search over the current plane; re-runs when the query, plane, or
  // search mode/provider change.
  $effect(() => {
    const query = q.trim()
    const params = { plane, q: query, semantic, provider: embedProvider }
    clearTimeout(timer)
    if (!query) {
      results = null
      return
    }
    timer = setTimeout(async () => {
      try {
        results = await rpc('plane.find', params)
      } catch (e) {
        results = { error: e.message }
      }
    }, 200)
  })

  function openNode(n) {
    focus = { kind: 'node', id: n.id, nonce: ++nonce }
    goExplore()
  }

  function openEdge(e) {
    focus = { kind: 'edge', edge: e, nonce: ++nonce }
    goExplore()
  }

  function goExplore() {
    view = 'explore'
    results = null
    q = ''
  }
</script>

<header>
  <h1 class="brand">
    <svg class="logo" viewBox="0 0 64 64" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" aria-hidden="true">
      <circle cx="32" cy="32" r="30" />
      <circle class="ticks" cx="32" cy="32" r="25.5" stroke-width="3" stroke-dasharray="1.2 3" />
      <rect x="16" y="16" width="32" height="32" />
      <rect x="16" y="16" width="32" height="32" transform="rotate(45 32 32)" />
      <circle cx="32" cy="32" r="11" />
      <circle cx="32" cy="32" r="3.5" fill="currentColor" stroke="none" />
    </svg>
    <span>Dr <b>STRANGE</b></span>
  </h1>

  <div class="tools">
    <label class="plane-pick">
      Plane
      <select bind:value={plane}>
        {#each planes as p (p.id)}<option value={p.name}>{p.name}</option>{/each}
        {#if !planes.length}<option value={plane}>{plane}</option>{/if}
      </select>
    </label>

    <div class="search">
      <input
        type="search"
        placeholder={semantic ? 'Search by meaning…' : 'Search this plane…'}
        bind:value={q}
        onkeydown={(e) => e.key === 'Escape' && (q = '')}
      />
    {#if results}
      <div class="results">
          {#if results.error}
            <p class="empty">{results.error}</p>
          {:else if !results.nodes.length && !results.edges?.length}
            {#if results.note}<p class="note">{results.note}</p>{/if}
            <p class="empty">no matches</p>
          {:else}
            {#if results.note}<p class="note">{results.note}</p>{/if}
            {#if results.nodes.length}
              <p class="group">Nodes</p>
              <ul>
                {#each results.nodes as n (n.id)}
                  <li>
                    <button onclick={() => openNode(n)}>
                      <span class="k">{n.external_key ?? `#${n.id}`}</span>
                      <span class="l">{n.labels?.join(', ')}</span>
                      <span class="m">{n.match}</span>
                    </button>
                  </li>
                {/each}
              </ul>
            {/if}
            {#if results.edges?.length}
              <p class="group">Edges</p>
              <ul>
                {#each results.edges as e (e.id)}
                  <li>
                    <button onclick={() => openEdge(e)}>
                      <span class="k">{e.type}</span>
                      <span class="l">#{e.src} → #{e.dst}</span>
                      <span class="m">{e.match}</span>
                    </button>
                  </li>
                {/each}
              </ul>
            {/if}
            {#if results.truncated}
              <p class="empty">more matches exist (scanned {results.scanned}/{results.total} nodes)</p>
            {/if}
          {/if}
        </div>
      {/if}
    </div>

    <label class="sem" title="Rank nodes by embedding similarity instead of substring matching">
      <input type="checkbox" bind:checked={semantic} /> semantic
    </label>
    {#if semantic}
      <select bind:value={embedProvider} title="Embedding provider (must match how the plane was embedded)">
        {#each EMBED_PROVIDERS as p (p)}<option value={p}>{p}</option>{/each}
      </select>
    {/if}
  </div>

  <nav>
    <button class:active={view === 'dashboard'} onclick={() => (view = 'dashboard')}>
      Dashboard
    </button>
    <button class:active={view === 'explore'} onclick={() => (view = 'explore')}>
      Explore
    </button>
    <button class:active={view === 'digest'} onclick={() => (view = 'digest')}>
      Digest
    </button>
  </nav>
</header>

<!-- Explore owns a sigma/WebGL instance created on mount and killed on
     destroy, so mounting views on demand keeps switching clean. -->
{#if view === 'dashboard'}
  <Dashboard {onPlaneCreated} />
{:else if view === 'explore'}
  <Explore {plane} {focus} />
{:else}
  <Digest {plane} />
{/if}
