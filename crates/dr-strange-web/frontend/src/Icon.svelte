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
    // dashboard stat boxes
    planes: '<path d="M12 2 2 7l10 5 10-5z"/><path d="M2 12l10 5 10-5"/><path d="M2 17l10 5 10-5"/>',
    nodes: '<circle cx="6" cy="6" r="2.5"/><circle cx="18" cy="9" r="2.5"/><circle cx="9" cy="18" r="2.5"/>',
    edges:
      '<path d="M9.5 14.5 14.5 9.5"/><path d="M14 6l1-1a3.5 3.5 0 0 1 5 5l-2 2"/><path d="M10 18l-1 1a3.5 3.5 0 0 1-5-5l2-2"/>',
    labels:
      '<path d="M20.6 13.4l-7.2 7.2a2 2 0 0 1-2.8 0l-7-7A2 2 0 0 1 3 12V4a1 1 0 0 1 1-1h8a2 2 0 0 1 1.4.6l7.2 7.2a2 2 0 0 1 0 2.6z"/><circle cx="7.5" cy="7.5" r="1.1"/>',
    edgetypes:
      '<polyline points="16 3 21 3 21 8"/><line x1="21" y1="3" x2="13.5" y2="10.5"/><polyline points="8 21 3 21 3 16"/><line x1="3" y1="21" x2="10.5" y2="13.5"/>',
    indexes: '<circle cx="11" cy="11" r="7"/><line x1="21" y1="21" x2="16.65" y2="16.65"/>',
    degree:
      '<circle cx="18" cy="5" r="2.5"/><circle cx="6" cy="12" r="2.5"/><circle cx="18" cy="19" r="2.5"/><line x1="8.1" y1="10.7" x2="15.9" y2="6.3"/><line x1="8.1" y1="13.3" x2="15.9" y2="17.7"/>',
    commits:
      '<circle cx="12" cy="12" r="3.5"/><line x1="3" y1="12" x2="8.5" y2="12"/><line x1="15.5" y1="12" x2="21" y2="12"/>',
    disk: '<ellipse cx="12" cy="5" rx="8" ry="3"/><path d="M4 5v6c0 1.7 3.6 3 8 3s8-1.3 8-3V5"/><path d="M4 11v6c0 1.7 3.6 3 8 3s8-1.3 8-3v-6"/>',
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
