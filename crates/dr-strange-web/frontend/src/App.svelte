<script>
  import { onMount } from 'svelte'
  import { rpc } from './rpc.js'
  import { loadPref, savePref } from './prefs.js'
  import Dashboard from './Dashboard.svelte'
  import Explore from './Explore.svelte'
  import Query from './Query.svelte'
  import Digest from './Digest.svelte'
  import Icon from './Icon.svelte'

  // Providers with an embedding endpoint (deepseek is chat-only, so excluded).
  const EMBED_PROVIDERS = ['openai', 'qwen', 'ollama']

  let view = $state('dashboard')
  let planes = $state([]) // [{ id, name, ... }]
  let plane = $state(loadPref('plane', 'startup')) // app-wide current plane
  let q = $state('') // search box
  let semantic = $state(false) // text substring vs embedding-similarity search
  let embedProvider = $state(loadPref('embedProvider', 'openai'))
  let results = $state(null) // { nodes, edges, mode, note, ... } | { error }
  let focus = $state(null) // { id, nonce } → Explore centers this node

  // Time-travel (ROADMAP §4): an app-wide "viewing as of" cursor. `plane.history`
  // answers only on a native server, so a successful probe both proves the
  // capability and gives the queryable commit window. When `asOf` is set, the
  // header search and the Explore plot both read that past snapshot.
  let timeTravel = $state(false) // native backend? (history probe succeeded)
  let history = $state(null) // { oldest, latest } commit-sequence window
  let asOf = $state(null) // null = live; else a commit seq everything reads at

  async function loadHistory() {
    try {
      const h = await rpc('plane.history')
      history = h
      timeTravel = true
      if (asOf != null && (asOf < h.oldest || asOf > h.latest)) asOf = null
    } catch {
      timeTravel = false
      history = null
      asOf = null
    }
  }

  // Snap the whole dashboard back to the live (latest) state.
  function goLive() {
    asOf = null
  }

  // Remember the current plane + provider across reloads.
  $effect(() => {
    savePref('plane', plane)
    savePref('embedProvider', embedProvider)
  })

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

  // Dashboard also deletes planes; refresh the picker, and leave the deleted
  // plane if it was the current one (startup always survives).
  function onPlaneDeleted(name) {
    loadPlanes()
    if (plane === name) plane = 'startup'
  }

  onMount(async () => {
    try {
      await loadPlanes()
      if (!planes.some((p) => p.name === plane)) plane = planes[0]?.name ?? 'startup'
    } catch {
      // Dashboard still works; the header just shows the default plane.
    }
  })

  // Probe the time-travel window / capability on load and whenever the plane
  // changes (the commit seq is DB-global, so any plane works as the trigger).
  $effect(() => {
    plane // track
    loadHistory()
  })

  // Debounced search over the current plane; re-runs when the query, plane,
  // search mode/provider, or the time-travel cursor change. When `asOf` is set
  // the search runs against that past snapshot (text scans it; semantic
  // brute-forces it, since the vector index only knows the latest commit).
  $effect(() => {
    const query = q.trim()
    const params = { plane, q: query, semantic, provider: embedProvider }
    if (asOf != null) params.as_of = asOf
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
        class:travelling={asOf != null}
        placeholder={asOf != null
          ? `Search as of commit ${asOf}…`
          : semantic
            ? 'Search by meaning…'
            : 'Quick search in this plane…'}
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
    {#if asOf != null}
      <button class="tt-chip" onclick={goLive} title="The whole view is pinned to a past commit — click to return to live">
        <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M3 3v5h5" />
          <path d="M3.05 13A9 9 0 1 0 6 5.3L3 8" />
        </svg>
        as of {asOf}{#if history} / {history.latest}{/if}
      </button>
    {/if}
  </div>

  <nav>
    <button class:active={view === 'dashboard'} onclick={() => (view = 'dashboard')}>
      <Icon name="dashboard" /> Dashboard
    </button>
    <button class:active={view === 'explore'} onclick={() => (view = 'explore')}>
      <Icon name="explore" /> Explore
    </button>
    <button class:active={view === 'query'} onclick={() => (view = 'query')}>
      <Icon name="query" /> Query
    </button>
    <button class:active={view === 'digest'} onclick={() => (view = 'digest')}>
      <Icon name="aigest" /> AIgest
    </button>
  </nav>
</header>

<!-- Explore owns a sigma/WebGL instance created on mount and killed on
     destroy, so mounting views on demand keeps switching clean. -->
{#if view === 'dashboard'}
  <Dashboard {plane} {onPlaneCreated} {onPlaneDeleted} onSelectPlane={(name) => (plane = name)} />
{:else if view === 'explore'}
  <Explore {plane} {focus} {onPlaneCreated} bind:asOf {history} {timeTravel} />
{:else if view === 'query'}
  <Query {plane} />
{:else}
  <Digest {plane} {onPlaneCreated} />
{/if}

<footer class="site-footer">
  <span class="foot-brand">
    <svg class="foot-logo" viewBox="0 0 64 64" fill="none" stroke="currentColor" stroke-width="3" stroke-linejoin="round" aria-hidden="true">
      <circle cx="32" cy="32" r="30" />
      <rect x="16" y="16" width="32" height="32" transform="rotate(45 32 32)" />
      <circle cx="32" cy="32" r="11" />
    </svg>
    Dr <b>STRANGE</b>
  </span>
  <span class="sep">·</span>
  <span>an AI-native embedded graph database</span>
  <span class="foot-right">
    <a href="https://github.com/wangyingsm/dr-strange" target="_blank" rel="noreferrer noopener">GitHub</a>
  </span>
</footer>
