<script>
  import Dashboard from './Dashboard.svelte'
  import Explore from './Explore.svelte'

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
  </nav>
</header>

<!-- Keep both mounted-on-demand: Explore owns a sigma/WebGL instance that is
     created on mount and killed on destroy, so switching views is clean. -->
{#if view === 'dashboard'}
  <Dashboard />
{:else}
  <Explore />
{/if}
