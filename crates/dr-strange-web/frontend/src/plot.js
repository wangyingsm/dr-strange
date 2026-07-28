// WebGL graph plot: a thin imperative wrapper over graphology (data model) +
// sigma (WebGL renderer) + ForceAtlas2 (layout), kept out of Svelte's reactive
// world since these objects mutate in place (arch/08 §2.2). The Explore view
// drives it: seed a subgraph, click a node to expand its neighbourhood.

import Graph from 'graphology'
import Sigma from 'sigma'
import forceAtlas2 from 'graphology-layout-forceatlas2'

// Categorical palette; labels are assigned colors in first-seen order so the
// legend stays stable within a session.
const PALETTE = [
  '#7c3aed', '#2563eb', '#059669', '#d97706', '#dc2626',
  '#0891b2', '#db2777', '#65a30d', '#9333ea', '#0d9488',
]

function colorFor(label, legend) {
  const key = label || '(none)'
  if (!legend.has(key)) legend.set(key, PALETTE[legend.size % PALETTE.length])
  return legend.get(key)
}

export class Plot {
  constructor(container, handlers = {}) {
    this.graph = new Graph({ type: 'directed', multi: true })
    this.legend = new Map()
    this.sigma = new Sigma(this.graph, container, {
      defaultEdgeType: 'arrow',
      enableEdgeEvents: true,
      renderEdgeLabels: true,
      labelDensity: 0.5,
      labelRenderedSizeThreshold: 5,
      // Edge picking reads a downsized framebuffer, so a thin edge has no
      // clickable pixels and the hit falls through to the stage. Give edges a
      // floor thickness so they can actually be selected/hovered.
      minEdgeThickness: 3.5,
    })
    this.sigma.on('clickNode', ({ node }) =>
      handlers.onSelectNode?.(node, this.graph.getNodeAttribute(node, 'record')),
    )
    this.sigma.on('clickEdge', ({ edge }) =>
      handlers.onSelectEdge?.(edge, this.graph.getEdgeAttribute(edge, 'record')),
    )
    this.sigma.on('clickStage', () => handlers.onClearSelection?.())
  }

  clear() {
    this.graph.clear()
    this.legend.clear()
  }

  /**
   * Merge a `{nodes, edges}` subgraph in. New nodes are seeded near `anchor`
   * (the clicked node) so an expansion grows outward instead of teleporting;
   * existing nodes keep their positions. Returns nothing — call sites read
   * `legendEntries()` afterward.
   */
  addSubgraph({ nodes = [], edges = [] }, anchorId = null) {
    const anchor =
      anchorId != null && this.graph.hasNode(String(anchorId))
        ? {
            x: this.graph.getNodeAttribute(String(anchorId), 'x'),
            y: this.graph.getNodeAttribute(String(anchorId), 'y'),
          }
        : { x: 0, y: 0 }

    for (const n of nodes) {
      const id = String(n.id)
      if (this.graph.hasNode(id)) continue
      const label = n.labels?.[0] ?? ''
      this.graph.addNode(id, {
        label: n.external_key ?? (n.labels?.length ? n.labels.join(', ') : id),
        x: anchor.x + (Math.random() - 0.5),
        y: anchor.y + (Math.random() - 0.5),
        size: 5,
        color: colorFor(label, this.legend),
        record: n,
      })
    }

    for (const e of edges) {
      const key = 'e' + e.id
      if (this.graph.hasEdge(key)) continue
      if (this.graph.hasNode(String(e.src)) && this.graph.hasNode(String(e.dst))) {
        this.graph.addEdgeWithKey(key, String(e.src), String(e.dst), {
          label: e.type,
          size: 4,
          record: e,
        })
      }
    }

    this._sizeByDegree()
    this._layout()
  }

  _sizeByDegree() {
    this.graph.forEachNode((node) => {
      const d = this.graph.degree(node)
      this.graph.setNodeAttribute(node, 'size', 4 + Math.min(14, Math.sqrt(d) * 2.5))
    })
  }

  _layout() {
    if (this.graph.order === 0) {
      this.sigma.refresh()
      return
    }
    forceAtlas2.assign(this.graph, {
      iterations: this.graph.order > 400 ? 60 : 150,
      settings: {
        gravity: 1.2,
        scalingRatio: 18,
        adjustSizes: true,
        barnesHutOptimize: this.graph.order > 400,
      },
    })
    this.sigma.refresh()
  }

  legendEntries() {
    return [...this.legend.entries()].map(([label, color]) => ({ label, color }))
  }

  destroy() {
    this.sigma.kill()
  }
}
