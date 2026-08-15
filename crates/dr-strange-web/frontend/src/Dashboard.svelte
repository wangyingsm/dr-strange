<script>
  import { onMount } from 'svelte'
  import { rpc, liveStats, authHeaders } from './rpc.js'
  import CreatePlane from './CreatePlane.svelte'
  import Icon from './Icon.svelte'

  let {
    plane = 'startup', // the app-wide current plane (for highlighting)
    onPlaneCreated = () => {},
    onPlaneDeleted = () => {},
    onSelectPlane = () => {},
  } = $props()

  let stats = $state(null) // live db.stats (planes/nodes/edges/file_size)
  let planes = $state([]) // plane cards
  import extensionLogo from './assets/extension-logo.svg'

  let plugins = $state([]) // installed preprocessor plugins
  let catalog = $state([]) // the official catalog this build pins
  let busy = $state({}) // plugin name -> true while an install/upgrade runs
  let installing = $state(false)
  let installUrl = $state('')

  // The Extensions section pins every official plugin (catalog order) and
  // judges each against the store by hash: installed, upgradable, or absent.
  let officials = $derived(
    catalog.map((c) => {
      const inst = plugins.find((p) => p.name === c.name)
      const state = !inst ? 'absent' : inst.sha256 === c.sha256 ? 'installed' : 'upgradable'
      return { ...c, inst, state }
    })
  )
  let thirdParty = $derived(plugins.filter((p) => !catalog.some((c) => c.name === p.name)))
  let error = $state(null)
  let connected = $state(false)

  let creating = $state(false) // new-plane popup open?

  // "Delete plane" popup (type-to-confirm).
  let deleting = $state(null) // the plane being deleted, or null
  let confirmName = $state('')
  let deleteError = $state(null)

  async function loadPlanes() {
    planes = await rpc('plane.list')
  }

  async function afterCreate(name) {
    await loadPlanes() // refresh the cards
    onPlaneCreated(name) // refresh the header picker + switch to it
  }

  // Download a plane as JSONL (the server builds it; drsg import reads it back).
  async function exportPlane(name) {
    error = null
    try {
      // POST (not GET): a same-origin GET omits the Origin header the server's
      // local-UI check needs, so a tokenless server would 401 its own UI.
      const res = await fetch(`/export?plane=${encodeURIComponent(name)}`, {
        method: 'POST',
        headers: authHeaders(),
      })
      if (!res.ok) throw new Error(`export failed (${res.status})`)
      const blob = await res.blob()
      const url = URL.createObjectURL(blob)
      const a = document.createElement('a')
      a.href = url
      a.download = `${name}.jsonl`
      document.body.appendChild(a)
      a.click()
      a.remove()
      URL.revokeObjectURL(url)
    } catch (e) {
      error = e.message
    }
  }

  function openDelete(p) {
    deleting = p
    confirmName = ''
    deleteError = null
  }
  function closeDelete() {
    deleting = null
  }

  async function submitDelete() {
    if (!deleting || confirmName.trim() !== deleting.name) return // type-to-confirm
    const name = deleting.name
    deleteError = null
    try {
      await rpc('plane.delete', { plane: name })
      await loadPlanes()
      onPlaneDeleted(name) // refresh the header picker + leave the plane if current
      deleting = null
    } catch (e) {
      deleteError = e.message
    }
  }

  // Focus the input as soon as a popup opens.
  function autofocus(el) {
    el.focus()
  }

  function onKeydown(e) {
    if (e.key === 'Escape' && deleting) closeDelete()
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

  const fmtNum = (n) => (typeof n === 'number' ? n.toLocaleString() : '—')

  // Mean undirected degree (each edge touches two nodes).
  let avgDegree = $derived.by(() =>
    stats?.nodes ? ((stats.edges * 2) / stats.nodes).toFixed(1) : '—',
  )

  async function loadPlugins() {
    plugins = await rpc('plugin.list')
  }

  function logoSrc(svg) {
    if (!svg) return extensionLogo
    return 'data:image/svg+xml;utf8,' + encodeURIComponent(svg)
  }

  async function installOfficial(c) {
    busy = { ...busy, [c.name]: true }
    try {
      await rpc('plugin.install', { url: c.url })
      await loadPlugins()
    } catch (e) {
      error = e.message
    } finally {
      busy = { ...busy, [c.name]: false }
    }
  }

  async function removePlugin(name) {
    if (!confirm(`Remove plugin ${name}? Files it handled will fall back to the document reader.`)) return
    try {
      await rpc('plugin.remove', { name })
      await loadPlugins()
    } catch (e) {
      error = e.message
    }
  }

  async function submitInstall() {
    const url = installUrl.trim()
    if (!url) return
    try {
      await rpc('plugin.install', { url })
      installing = false
      installUrl = ''
      await loadPlugins()
    } catch (e) {
      error = e.message
    }
  }

  onMount(() => {
    rpc('db.stats')
      .then((s) => (stats = s))
      .catch((e) => (error = e.message))
    loadPlanes().catch((e) => (error = e.message))
    loadPlugins().catch((e) => (error = e.message))
    rpc('plugin.catalog')
      .then((c) => (catalog = c))
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

<svelte:window onkeydown={onKeydown} />

<div class="row">
  <span class="conn" class:on={connected}>{connected ? 'live' : 'offline'}</span>
</div>

{#if error}
  <p class="error">{error}</p>
{/if}

<section class="health">
  <div class="stat"><Icon name="planes" size={22} /><div class="v"><span class="n">{fmtNum(stats?.planes)}</span><span class="l">planes</span></div></div>
  <div class="stat"><Icon name="nodes" size={22} /><div class="v"><span class="n">{fmtNum(stats?.nodes)}</span><span class="l">nodes</span></div></div>
  <div class="stat"><Icon name="edges" size={22} /><div class="v"><span class="n">{fmtNum(stats?.edges)}</span><span class="l">edges</span></div></div>
  <div class="stat"><Icon name="labels" size={22} /><div class="v"><span class="n">{fmtNum(stats?.labels)}</span><span class="l">labels</span></div></div>
  <div class="stat"><Icon name="edgetypes" size={22} /><div class="v"><span class="n">{fmtNum(stats?.edge_types)}</span><span class="l">edge types</span></div></div>
  <div class="stat"><Icon name="indexes" size={22} /><div class="v"><span class="n">{fmtNum(stats?.indexes)}</span><span class="l">indexes</span></div></div>
  <div class="stat"><Icon name="degree" size={22} /><div class="v"><span class="n">{avgDegree}</span><span class="l">avg degree</span></div></div>
  <div class="stat"><Icon name="commits" size={22} /><div class="v"><span class="n">{fmtNum(stats?.commit_seq)}</span><span class="l">commits</span></div></div>
  <div class="stat">
    <Icon name="disk" size={22} />
    <div class="v">
      <span class="n">{stats?.persistent ? fmtBytes(stats?.file_size) : 'in-mem'}</span>
      <span class="l">on disk</span>
    </div>
  </div>
</section>

<h2>Planes</h2>
<section class="planes">
  {#each planes as p (p.id)}
    <article class="card" class:selected={p.name === plane}>
      <button
        class="card-select"
        aria-pressed={p.name === plane}
        title="Select this plane"
        onclick={() => onSelectPlane(p.name)}
      >
        <h3>{p.name}</h3>
        {#if p.properties?.description}
          <p class="desc">{propValue(p.properties.description)}</p>
        {/if}
        <div class="counts">
          <span>{p.nodes} nodes</span>
          <span>{p.edges} edges</span>
        </div>
      </button>
      <div class="card-actions">
        <button class="export" onclick={() => exportPlane(p.name)} title="Export this plane as JSONL">
          <svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M8 2.5v7" />
            <path d="M5 6.5 8 9.5l3-3" />
            <path d="M2.75 11.5v1.25a.75.75 0 0 0 .75.75h9a.75.75 0 0 0 .75-.75V11.5" />
          </svg>
          Export
        </button>
        <button
          class="del"
          onclick={() => openDelete(p)}
          disabled={p.name === 'startup'}
          title={p.name === 'startup' ? 'The startup plane cannot be deleted' : 'Delete this plane'}
        >
          <svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M2.75 4.25h10.5" />
            <path d="M6.25 4.25V2.75h3.5v1.5" />
            <path d="M4.35 4.25l.55 8.4a1 1 0 0 0 1 .9h4.2a1 1 0 0 0 1-.9l.55-8.4" />
            <path d="M6.75 6.75v4M9.25 6.75v4" />
          </svg>
          Delete
        </button>
      </div>
    </article>
  {/each}
  <button class="card new-card" onclick={() => (creating = true)} title="Create a new plane" aria-label="Create a new plane">
    <span aria-hidden="true">+</span>
    <span class="new-card-label">New plane</span>
  </button>
</section>

<h2>Extensions</h2>
<section class="plugins">
  {#each officials as c (c.name)}
    <article class="card plugin-card">
      <h3>
        <img class="plugin-logo" src={logoSrc(c.inst?.logo)} alt="" />
        {c.name}{#if c.inst}<span class="plugin-ver">@{c.inst.version}</span>{/if}
        {#if c.state === 'installed'}<span class="badge ok">installed</span>
        {:else if c.state === 'upgradable'}<span class="badge up">upgradable</span>{/if}
      </h3>
      <p class="plugin-exts">{c.claims}</p>
      {#if c.inst}
        <p class="plugin-sha" title={c.inst.source}>sha256:{c.inst.sha256.slice(0, 12)}</p>
      {:else}
        <p class="plugin-sha" title={c.url}>official release</p>
      {/if}
      <div class="card-actions">
        {#if c.state === 'absent'}
          <button class="install" onclick={() => installOfficial(c)} disabled={busy[c.name]}>
            {busy[c.name] ? 'installing…' : 'Install'}
          </button>
        {:else if c.state === 'upgradable'}
          <button class="install" onclick={() => installOfficial(c)} disabled={busy[c.name]}>
            {busy[c.name] ? 'installing…' : 'Upgrade'}
          </button>
          <button class="del" onclick={() => removePlugin(c.name)} disabled={busy[c.name]}>Remove</button>
        {:else}
          <button class="del" onclick={() => removePlugin(c.name)} disabled={busy[c.name]}>Remove</button>
        {/if}
      </div>
    </article>
  {/each}
  {#each thirdParty as p (p.name)}
    <article class="card plugin-card">
      <h3><img class="plugin-logo" src={logoSrc(p.logo)} alt="" />{p.name}<span class="plugin-ver">@{p.version}</span></h3>
      <p class="plugin-exts">{p.extensions.map((e) => '.' + e).join(' ')}</p>
      <p class="plugin-sha" title={p.source}>sha256:{p.sha256.slice(0, 12)}</p>
      <div class="card-actions">
        <button class="del" onclick={() => removePlugin(p.name)}>Remove</button>
      </div>
    </article>
  {/each}
  <button class="card new-card" onclick={() => (installing = true)} title="Install a plugin from a URL" aria-label="Install a plugin">
    <span aria-hidden="true">+</span>
    <span class="new-card-label">Install plugin</span>
  </button>
</section>

{#if installing}
  <div class="dlg-backdrop">
    <div class="dlg" role="dialog" aria-modal="true" aria-label="Install plugin">
      <header>
        Install plugin
        <button class="close" onclick={() => (installing = false)} aria-label="Close">×</button>
      </header>
      <div class="dlg-body">
        <p>
          Paste a plugin <code>.wasm</code> URL — official releases live at
          <a href="https://github.com/wangyingsm/dr-strange-extension/releases" target="_blank" rel="noreferrer">dr-strange-extension</a>.
          The artifact is validated and its hash pinned before anything runs.
        </p>
        <input
          type="text"
          placeholder="https://…/rust.wasm"
          bind:value={installUrl}
          onkeydown={(e) => e.key === 'Enter' && submitInstall()}
        />
        <div class="dlg-actions">
          <button onclick={() => (installing = false)}>Cancel</button>
          <button class="primary" onclick={submitInstall} disabled={!installUrl.trim()}>Install</button>
        </div>
      </div>
    </div>
  </div>
{/if}

<CreatePlane bind:open={creating} onCreated={afterCreate} />

{#if deleting}
  <div class="dlg-backdrop">
    <div class="dlg" role="dialog" aria-modal="true" aria-label="Delete plane {deleting.name}">
      <header>
        Delete plane
        <button class="close" onclick={closeDelete} aria-label="Close">×</button>
      </header>
      <div class="dlg-body">
        <p class="dlg-warn">
          This permanently deletes <b>{deleting.name}</b> and all its nodes and edges. This cannot be undone.
        </p>
        <label class="dlg-confirm">
          Type <code>{deleting.name}</code> to confirm
          <input
            type="text"
            placeholder={deleting.name}
            bind:value={confirmName}
            use:autofocus
            onkeydown={(e) => e.key === 'Enter' && submitDelete()}
          />
        </label>
        {#if deleteError}<p class="dlg-error">{deleteError}</p>{/if}
        <div class="dlg-actions">
          <button class="ghost" onclick={closeDelete}>Cancel</button>
          <button class="danger" onclick={submitDelete} disabled={confirmName.trim() !== deleting.name}>
            Delete
          </button>
        </div>
      </div>
    </div>
  </div>
{/if}
