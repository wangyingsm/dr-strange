<script>
  import { onMount } from 'svelte'
  import { rpc, liveStats } from './rpc.js'

  let { onPlaneCreated = () => {} } = $props()

  let stats = $state(null) // live db.stats (planes/nodes/edges/file_size)
  let planes = $state([]) // plane cards
  let error = $state(null)
  let connected = $state(false)

  // "New plane" popup.
  let creating = $state(false)
  let newName = $state('')
  let createError = $state(null)

  async function loadPlanes() {
    planes = await rpc('plane.list')
  }

  function openCreate() {
    newName = ''
    createError = null
    creating = true
  }
  function closeCreate() {
    creating = false
  }

  async function submitCreate() {
    const name = newName.trim()
    if (!name) return
    createError = null
    try {
      await rpc('plane.create', { name })
      await loadPlanes()
      onPlaneCreated(name) // refresh the header picker + switch to it
      creating = false
    } catch (e) {
      createError = e.message
    }
  }

  // Focus the input as soon as the popup opens.
  function autofocus(el) {
    el.focus()
  }

  function onKeydown(e) {
    if (creating && e.key === 'Escape') closeCreate()
  }

  // A property value is either a raw JSON value or, when it carries a
  // PropDesc description, `{ $desc, $value }` (core's json dialect).
  function propValue(v) {
    return v != null && typeof v === 'object' && '$value' in v ? v.$value : v
  }

  function fmtBytes(n) {
    if (n == null) return '—'
    const u = ['B', 'KB', 'MB', 'GB', 'TB']
    let i = 0
    while (n >= 1024 && i < u.length - 1) {
      n /= 1024
      i++
    }
    return `${n.toFixed(i === 0 ? 0 : 1)} ${u[i]}`
  }

  onMount(() => {
    rpc('db.stats')
      .then((s) => (stats = s))
      .catch((e) => (error = e.message))
    loadPlanes().catch((e) => (error = e.message))
    return liveStats(
      (s) => {
        stats = s
        error = null
      },
      (open) => (connected = open),
    )
  })
</script>

<svelte:window onkeydown={onKeydown} />

<div class="row">
  <span class="conn" class:on={connected}>{connected ? 'live' : 'offline'}</span>
</div>

{#if error}
  <p class="error">{error}</p>
{/if}

<section class="health">
  <div class="stat"><span class="n">{stats?.planes ?? '—'}</span><span class="l">planes</span></div>
  <div class="stat"><span class="n">{stats?.nodes ?? '—'}</span><span class="l">nodes</span></div>
  <div class="stat"><span class="n">{stats?.edges ?? '—'}</span><span class="l">edges</span></div>
  <div class="stat">
    <span class="n">{stats?.persistent ? fmtBytes(stats?.file_size) : 'in-mem'}</span>
    <span class="l">on disk</span>
  </div>
</section>

<h2>Planes</h2>
<section class="planes">
  {#each planes as p (p.id)}
    <article class="card">
      <h3>{p.name}</h3>
      {#if p.properties?.description}
        <p class="desc">{propValue(p.properties.description)}</p>
      {/if}
      <div class="counts">
        <span>{p.nodes} nodes</span>
        <span>{p.edges} edges</span>
      </div>
    </article>
  {/each}
  <button class="card new-card" onclick={openCreate} title="Create a new plane" aria-label="Create a new plane">
    <span aria-hidden="true">+</span>
    <span class="new-card-label">New plane</span>
  </button>
</section>

{#if creating}
  <div class="dlg-backdrop">
    <div class="dlg" role="dialog" aria-modal="true" aria-label="Create a new plane">
      <header>
        New plane
        <button class="close" onclick={closeCreate} aria-label="Close">×</button>
      </header>
      <div class="dlg-body">
        <input
          type="text"
          placeholder="plane name"
          bind:value={newName}
          use:autofocus
          onkeydown={(e) => e.key === 'Enter' && submitCreate()}
        />
        {#if createError}<p class="dlg-error">{createError}</p>{/if}
        <div class="dlg-actions">
          <button class="ghost" onclick={closeCreate}>Cancel</button>
          <button class="primary" onclick={submitCreate} disabled={!newName.trim()}>Create</button>
        </div>
      </div>
    </div>
  </div>
{/if}
