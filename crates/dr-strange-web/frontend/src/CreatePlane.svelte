<script>
  // Shared "new plane" popup. Control it with `bind:open`; on success it calls
  // `onCreated(name)`. Styling comes from the global .dlg-* rules in app.css.
  import { rpc } from './rpc.js'

  let { open = $bindable(false), onCreated = () => {} } = $props()

  let name = $state('')
  let error = $state(null)

  // Clear the field each time the popup opens.
  $effect(() => {
    if (open) {
      name = ''
      error = null
    }
  })

  function autofocus(el) {
    el.focus()
  }
  function close() {
    open = false
  }

  async function submit() {
    const n = name.trim()
    if (!n) return
    error = null
    try {
      await rpc('plane.create', { name: n })
      onCreated(n)
      open = false
    } catch (e) {
      error = e.message
    }
  }

  function onKeydown(e) {
    if (open && e.key === 'Escape') close()
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#if open}
  <div class="dlg-backdrop">
    <div class="dlg" role="dialog" aria-modal="true" aria-label="Create a new plane">
      <header>
        New plane
        <button class="close" onclick={close} aria-label="Close">×</button>
      </header>
      <div class="dlg-body">
        <input
          type="text"
          placeholder="plane name"
          bind:value={name}
          use:autofocus
          onkeydown={(e) => e.key === 'Enter' && submit()}
        />
        {#if error}<p class="dlg-error">{error}</p>{/if}
        <div class="dlg-actions">
          <button class="ghost" onclick={close}>Cancel</button>
          <button class="primary" onclick={submit} disabled={!name.trim()}>Create</button>
        </div>
      </div>
    </div>
  </div>
{/if}
