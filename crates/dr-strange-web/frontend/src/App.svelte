<script>
  import Dashboard from './Dashboard.svelte'
  import Explore from './Explore.svelte'
  import Digest from './Digest.svelte'

  let view = $state('dashboard')
</script>

<header>
  <h1>dr-strange</h1>
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
  <Explore />
{:else}
  <Digest />
{/if}
