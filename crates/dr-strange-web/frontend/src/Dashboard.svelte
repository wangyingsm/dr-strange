<script>
  import { onMount } from 'svelte'
  import { rpc, liveStats } from './rpc.js'

  let stats = $state(null) // live db.stats (planes/nodes/edges/file_size)
  let planes = $state([]) // plane cards
  let error = $state(null)
  let connected = $state(false)

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
    rpc('plane.list')
      .then((p) => (planes = p))
      .catch((e) => (error = e.message))
    return liveStats(
      (s) => {
        stats = s
        error = null
      },
      (open) => (connected = open),
    )
  })
</script>

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
  {:else}
    <p class="empty">No planes yet.</p>
  {/each}
</section>
