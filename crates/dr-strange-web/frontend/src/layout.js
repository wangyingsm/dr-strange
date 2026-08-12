// Graph reasoning behind the plot's legibility: which neighbours are leaves,
// which hubs have too many to draw, how a fan is arranged, and how far each
// node sits from what the reader selected.
//
// Kept apart from `plot.js` because none of it needs a renderer — it is
// arithmetic over a graphology instance — and because that is what makes it
// testable: importing sigma pulls in WebGL at module load.

// A hub's leaves — neighbours attached to it and nothing else — are the bulk of
// a hairball and carry the least structure: each says only "this exists, and it
// hangs off that". Past this many they fold into one bead the reader can open,
// so a core node with 200 satellites plots as a core node with one.
export const COLLAPSE_MIN = 20
// Below the fold threshold, leaves are still worth arranging: grouped by label
// into angular sectors, a ring reads like a pie chart instead of a smear.
export const SECTOR_MIN = 5

// How far the focus ring reaches before everything else recedes. Two hops:
// far enough to show a selection's context, near enough that the context is
// still a neighbourhood rather than the whole canvas.
export const FOCUS_HOPS = 2
// Opacity by hop distance from the selection; anything further gets FAR.
const FOCUS_ALPHA = [1, 1, 0.4]
const FAR_ALPHA = 0.12

/**
 * Re-express a colour at `alpha`. Fading rather than recolouring keeps a node's
 * category legible while it recedes, so the legend still means something in the
 * dimmed part of the canvas.
 */
export function dim(color, alpha) {
  if (alpha >= 1 || typeof color !== 'string') return color
  if (color.startsWith('#')) {
    const hex = color.slice(1)
    const full = hex.length === 3 ? [...hex].map((c) => c + c).join('') : hex
    const [r, g, b] = [0, 2, 4].map((i) => parseInt(full.slice(i, i + 2), 16))
    return `rgba(${r},${g},${b},${alpha})`
  }
  if (color.startsWith('rgba(')) return color // already transparent by intent
  if (color.startsWith('rgb(')) return color.replace('rgb(', 'rgba(').replace(')', `,${alpha})`)
  return color
}

/**
 * A node's leaves: neighbours joined to it and to nothing else. Counted by
 * distinct neighbours rather than degree, so a leaf carrying two parallel edges
 * to the same hub is still a leaf. Beads are never leaves — one already stands
 * for a fold and must not be folded again.
 */
export function leavesOf(graph, hub) {
  const out = []
  graph.forEachNeighbor(hub, (nb, attrs) => {
    if (attrs.bead) return
    const nbs = graph.neighbors(nb)
    if (nbs.length === 1 && nbs[0] === hub) out.push(nb)
  })
  return out
}

/**
 * Which hubs have more leaves than are worth drawing, and which leaves those
 * are. The decision only — folding them away needs their records, which belong
 * to the plot.
 */
export function hubsToFold(graph, { min = COLLAPSE_MIN, skipHubs = new Set(), keep = new Set() } = {}) {
  const work = []
  graph.forEachNode((hub, attrs) => {
    if (attrs.bead || skipHubs.has(hub)) return
    // Never fold away what the reader is looking at.
    const leaves = leavesOf(graph, hub).filter((n) => !keep.has(n))
    if (leaves.length >= min) work.push([hub, leaves])
  })
  return work
}

/**
 * Lay each hub's leaves on a ring around it, grouped by label into contiguous
 * sectors. ForceAtlas2 has no reason to prefer any arrangement of nodes whose
 * only tie is to one hub, so it produces an even smear; grouping them turns
 * that ring into something with regions, and the legend starts to pay off.
 *
 * Only leaves move. They have exactly one neighbour, so no other structure
 * depends on where they sit and nothing FA2 worked out for the rest of the
 * graph is disturbed. The ring keeps the radius FA2 chose — positions are in
 * its arbitrary units, not pixels — grown for a crowded fan so the extra nodes
 * have somewhere to be.
 */
export function sectorLeaves(graph, min = SECTOR_MIN) {
  graph.forEachNode((hub, attrs) => {
    if (attrs.bead) return
    const leaves = leavesOf(graph, hub)
    if (leaves.length < min) return

    const { x: hx, y: hy } = attrs
    const radius =
      leaves.reduce(
        (sum, n) =>
          sum + Math.hypot(graph.getNodeAttribute(n, 'x') - hx, graph.getNodeAttribute(n, 'y') - hy),
        0,
      ) / leaves.length || 1

    // Biggest sector first, ties broken by name, so the same graph always
    // draws the same way.
    const groups = new Map()
    for (const n of leaves) {
      const label = graph.getNodeAttribute(n, 'record')?.labels?.[0] ?? ''
      if (!groups.has(label)) groups.set(label, [])
      groups.get(label).push(n)
    }
    const ordered = [...groups.entries()].sort(
      (a, b) => b[1].length - a[1].length || a[0].localeCompare(b[0]),
    )

    const r = radius * Math.max(1, Math.sqrt(leaves.length / min))
    let i = 0
    for (const [, members] of ordered) {
      members.sort()
      for (const n of members) {
        const angle = (2 * Math.PI * i) / leaves.length
        graph.setNodeAttribute(n, 'x', hx + r * Math.cos(angle))
        graph.setNodeAttribute(n, 'y', hy + r * Math.sin(angle))
        i++
      }
    }
  })
}

/**
 * Hops from the focus, breadth-first, stopping at `hops`. Nodes beyond it are
 * absent from the map rather than carrying a large number — "further than we
 * measured" is the only thing the renderer needs to know about them.
 */
/**
 * The frontier: entities with at most one edge *on the canvas*, plus every
 * entity currently folded into a bead.
 *
 * These are where the picture stops rather than where the graph does — a node
 * drawn with one connection is almost always one whose other neighbours were
 * never fetched.
 *
 * **The folded ones are the point.** `hubsToFold` drops a hub's leaves from the
 * graph and leaves a bead behind, so after any seed most of the frontier is
 * inside beads rather than in the graph. Reading only the graph reports an
 * empty frontier on exactly the views that have the largest one, and an
 * "expand" built on it appears to do nothing at all.
 *
 * @param graph the plotted graphology graph
 * @param collapsed the plot's `bead key -> { hub, nodes, edges }` map
 */
export function frontierIds(graph, collapsed = new Map()) {
  const out = new Set()
  graph.forEachNode((node, attrs) => {
    if (!attrs.bead && graph.degree(node) <= 1) out.add(node)
  })
  for (const { nodes } of collapsed.values()) {
    for (const record of nodes ?? []) {
      if (record?.id != null) out.add(String(record.id))
    }
  }
  return [...out]
}

export function focusDistances(graph, focusNodes, hops = FOCUS_HOPS) {
  const dist = new Map()
  let frontier = [...focusNodes].filter((n) => graph.hasNode(n))
  for (const n of frontier) dist.set(n, 0)
  for (let d = 1; d <= hops && frontier.length; d++) {
    const next = []
    for (const cur of frontier) {
      graph.forEachNeighbor(cur, (nb) => {
        if (!dist.has(nb)) {
          dist.set(nb, d)
          next.push(nb)
        }
      })
    }
    frontier = next
  }
  return dist
}


/**
 * How hard importance pushes a fan open. ForceAtlas2's attraction reads an
 * edge's `weight`, and a heavier edge ends up LONGER rather than shorter —
 * weight also feeds each node's mass, and the mass term wins. Measured on a
 * synthetic star, as the gap between adjacent leaves in node radii (the only
 * scale-invariant measure, since sigma re-frames the camera every draw):
 *
 *   fan of 20   k=0: 6.6   k=3: 9.8    k=5: 12.3   k=12: 16.3
 *   fan of 40   k=0: 8.7   k=3: 12.5   k=5: 13.8   k=12: 13.1
 *   fan of 80   k=0: 10.6  k=3: 10.4   k=5: 10.4   k=12: 9.8
 *
 * 3 is the value that helps everywhere it can: about +45% at twenty and forty
 * leaves, and level at eighty. The ceiling is real — past that many the ring is
 * angularly saturated and no amount of pushing outward buys legibility, which
 * is what the fold is for.
 */
export const IMPORTANCE_SPREAD = 3

/**
 * Weight every edge by the importance of its more important end, so the
 * layout gives a hub's neighbourhood room instead of packing it against the
 * hub. `scores` are raw; they are normalised here, so any scale works.
 */
export function weightByImportance(graph, scores, k = IMPORTANCE_SPREAD) {
  const top = Math.max(...scores.values(), 0)
  const norm = (n) => (top > 0 ? (scores.get(n) ?? 0) / top : 0)
  graph.forEachEdge((edge, _attrs, source, target) => {
    graph.setEdgeAttribute(edge, 'weight', 1 + k * Math.max(norm(source), norm(target)))
  })
}

/** Opacity for a node at hop distance `d` from the focus; `null` means far. */
export function alphaFor(d) {
  return d == null ? FAR_ALPHA : (FOCUS_ALPHA[d] ?? FAR_ALPHA)
}
