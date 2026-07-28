<script>
  import { onMount } from 'svelte'
  import { rpc } from './rpc.js'
  import Dashboard from './Dashboard.svelte'
  import Explore from './Explore.svelte'
  import Digest from './Digest.svelte'

  let view = $state('dashboard')
  let planes = $state([]) // [{ id, name, ... }]
  let plane = $state('startup') // app-wide current plane (browse + search)
  let q = $state('') // search box
  let results = $state(null) // { nodes, scanned, total, truncated } | { error }
  let focus = $state(null) // { id, nonce } → Explore centers this node

  let nonce = 0
  let timer

  onMount(async () => {
    try {
      planes = await rpc('plane.list')
      if (!planes.some((p) => p.name === plane)) plane = planes[0]?.name ?? 'startup'
    } catch {
      // Dashboard still works; the header just shows the default plane.
    }
  })

  // Debounced text search over the current plane.
  $effect(() => {
    const query = q.trim()
    const pl = plane
    clearTimeout(timer)
    if (!query) {
      results = null
      return
    }
    timer = setTimeout(async () => {
      try {
        results = await rpc('plane.find', { plane: pl, q: query })
      } catch (e) {
        results = { error: e.message }
      }
    }, 200)
  })

  function openNode(n) {
    focus = { id: n.id, nonce: ++nonce }
    view = 'explore'
    results = null
    q = ''
  }
</script>

<header>
  <h1>dr-strange</h1>

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
        placeholder="Search this plane…"
        bind:value={q}
        onkeydown={(e) => e.key === 'Escape' && (q = '')}
      />
      {#if results}
        <div class="results">
          {#if results.error}
            <p class="empty">{results.error}</p>
          {:else if !results.nodes.length}
            <p class="empty">no matches</p>
          {:else}
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
            {#if results.truncated}
              <p class="empty">
                first {results.nodes.length} shown · more exist (scanned {results.scanned}/{results.total})
              </p>
            {/if}
          {/if}
        </div>
      {/if}
    </div>
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
  <Dashboard />
{:else if view === 'explore'}
  <Explore {plane} {focus} />
{:else}
  <Digest />
{/if}
