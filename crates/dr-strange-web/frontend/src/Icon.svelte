<script>
  // Small inline stroke icons (24×24, currentColor) used on menu items and
  // Explore tabs. `name` picks the glyph; markup is static + trusted.
  let { name, size = 15 } = $props()

  const ICONS = {
    // top-nav
    dashboard:
      '<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/>',
    explore:
      '<circle cx="6" cy="7" r="2.5"/><circle cx="18" cy="8" r="2.5"/><circle cx="12" cy="18" r="2.5"/><line x1="8.2" y1="8.2" x2="9.9" y2="16"/><line x1="15.8" y1="9" x2="13.5" y2="16.4"/><line x1="8.4" y1="7.4" x2="15.6" y2="7.7"/>',
    aigest:
      '<path d="M11 3l1.4 5.6L18 10l-5.6 1.4L11 17l-1.4-5.6L4 10l5.6-1.4z"/><path d="M18 14l.5 2 2 .5-2 .5-.5 2-.5-2-2-.5 2-.5z"/>',
    // explore tabs
    filters: '<path d="M3 5h18l-7 8.5V20l-4 1v-7.5z"/>',
    graphql:
      '<polyline points="4 6 8.5 12 4 18"/><line x1="11.5" y1="18" x2="20" y2="18"/>',
    algorithms: '<polyline points="3 12 7 12 10 5 14 19 17 12 21 12"/>',
    hybrid:
      '<path d="M12 2 3 6.5l9 4.5 9-4.5z"/><path d="M3 12l9 4.5 9-4.5"/><path d="M3 17l9 4.5 9-4.5"/>',
    ask: '<path d="M21 11.5a8.5 8.5 0 0 1-12.3 7.6L3 21l1.9-5.7A8.5 8.5 0 1 1 21 11.5z"/>',
    timetravel: '<path d="M3 4v5h5"/><path d="M3.5 13A9 9 0 1 0 6 6L3 9"/><path d="M12 8v4.5l3 1.8"/>',
    live: '<circle cx="12" cy="12" r="2.2"/><path d="M8 8a6 6 0 0 0 0 8M16 8a6 6 0 0 1 0 8M5.5 5.5a10 10 0 0 0 0 13M18.5 5.5a10 10 0 0 1 0 13"/>',
  }
</script>

<svg
  class="icon"
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width="1.9"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true">{@html ICONS[name] ?? ''}</svg>
