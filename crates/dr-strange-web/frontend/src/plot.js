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

const HOVER_COLOR = '#f59e0b' // amber — visible on both light and dark

// A theme-aware replacement for sigma's default node-hover drawer, whose label
// box is hardcoded white (invisible light text on it in dark mode). Same box
// geometry as the default; only the fill + label color are themed. `label` and
// `bg` are getters read on every hover, so it tracks live theme changes.
function drawNodeHover(label, bg) {
  return (context, data, settings) => {
    const { labelSize: size, labelFont: font, labelWeight: weight } = settings
    context.font = `${weight} ${size}px ${font}`
    context.fillStyle = bg()
    context.shadowOffsetX = 0
    context.shadowOffsetY = 0
    context.shadowBlur = 8
    context.shadowColor = '#000'
    const PADDING = 2
    if (typeof data.label === 'string') {
      const boxWidth = Math.round(context.measureText(data.label).width + 5)
      const boxHeight = Math.round(size + 2 * PADDING)
      const radius = Math.max(data.size, size / 2) + PADDING
      const angle = Math.asin(boxHeight / 2 / radius)
      const dx = Math.sqrt(Math.abs(radius ** 2 - (boxHeight / 2) ** 2))
      context.beginPath()
      context.moveTo(data.x + dx, data.y + boxHeight / 2)
      context.lineTo(data.x + radius + boxWidth, data.y + boxHeight / 2)
      context.lineTo(data.x + radius + boxWidth, data.y - boxHeight / 2)
      context.lineTo(data.x + dx, data.y - boxHeight / 2)
      context.arc(data.x, data.y, radius, angle, -angle)
      context.closePath()
      context.fill()
    } else {
      context.beginPath()
      context.arc(data.x, data.y, data.size + PADDING, 0, Math.PI * 2)
      context.closePath()
      context.fill()
    }
    context.shadowBlur = 0
    if (data.label) {
      context.fillStyle = label()
      context.fillText(data.label, data.x + data.size + 3, data.y + size / 3)
    }
  }
}

export class Plot {
  constructor(container, handlers = {}) {
    this.graph = new Graph({ type: 'directed', multi: true })
    this.legend = new Map()
    this.container = container
    this.hoveredEdge = null
    this.selectedEdge = null // persists (unlike hover) while an edge is selected
    this.selectedNode = null // the selected node, shown focused in the graph

    // Labels are canvas-drawn, so CSS variables don't reach them — read the
    // OS theme directly and keep them legible on both backgrounds.
    const media = window.matchMedia('(prefers-color-scheme: dark)')
    const nodeLabel = () => (media.matches ? '#e8e8ee' : '#1a1a1e')
    const edgeLabel = () => (media.matches ? '#9ca3af' : '#6b7280')
    // Hover-label box: sigma's default paints it hardcoded white, hiding our
    // light dark-mode label text. Match the panel background per theme instead.
    const hoverBg = () => (media.matches ? '#26262e' : '#ffffff')

    this.sigma = new Sigma(this.graph, container, {
      defaultEdgeType: 'arrow',
      enableEdgeEvents: true,
      renderEdgeLabels: true,
      labelColor: { color: nodeLabel() },
      edgeLabelColor: { color: edgeLabel() },
      defaultDrawNodeHover: drawNodeHover(nodeLabel, hoverBg),
      labelDensity: 0.5,
      labelRenderedSizeThreshold: 5,
      // Edge picking reads a downsized framebuffer, so a thin edge has no
      // clickable pixels and the hit falls through to the stage. Give edges a
      // floor thickness so they can actually be selected/hovered.
      minEdgeThickness: 3.5,
      // Highlight the hovered OR selected edge — thicker, amber, with its type
      // label forced visible. Hover makes the clickable region obvious; the
      // selected edge stays lit so a click/search focus is visible.
      edgeReducer: (edge, data) =>
        edge === this.hoveredEdge || edge === this.selectedEdge
          ? { ...data, size: (data.size ?? 4) * 1.8, color: HOVER_COLOR, forceLabel: true, zIndex: 1 }
          : data,
      // The selected node renders focused: `highlighted` routes it through the
      // (themed) hover box, plus a size bump — its category color is kept.
      nodeReducer: (node, data) =>
        node === this.selectedNode
          ? { ...data, highlighted: true, forceLabel: true, size: (data.size ?? 5) * 1.35, zIndex: 1 }
          : data,
    })

    this.sigma.on('clickNode', ({ node }) => {
      this.selectedNode = node
      this.selectedEdge = null // a node click supersedes any edge selection
      this.sigma.refresh()
      handlers.onSelectNode?.(node, this.graph.getNodeAttribute(node, 'record'))
    })
    this.sigma.on('clickEdge', ({ edge }) => {
      this.selectedEdge = edge
      this.selectedNode = null
      this.sigma.refresh()
      handlers.onSelectEdge?.(edge, this.graph.getEdgeAttribute(edge, 'record'))
    })
    this.sigma.on('clickStage', () => {
      this.selectedEdge = null
      this.selectedNode = null
      this.sigma.refresh()
      handlers.onClearSelection?.()
    })

    // Recolour labels if the OS theme flips while the plot is open.
    media.addEventListener('change', () => {
      this.sigma.setSetting('labelColor', { color: nodeLabel() })
      this.sigma.setSetting('edgeLabelColor', { color: edgeLabel() })
    })

    // Pointer cursor + edge highlight on hover.
    this.sigma.on('enterNode', () => this._cursor('pointer'))
    this.sigma.on('leaveNode', () => this._cursor(''))
    this.sigma.on('enterEdge', ({ edge }) => {
      this.hoveredEdge = edge
      this._cursor('pointer')
      this.sigma.refresh()
    })
    this.sigma.on('leaveEdge', () => {
      this.hoveredEdge = null
      this._cursor('')
      this.sigma.refresh()
    })
  }

  _cursor(value) {
    this.container.style.cursor = value
  }

  clear() {
    this.graph.clear()
    this.legend.clear()
    this.selectedEdge = null
    this.selectedNode = null
  }

  /** Highlight an edge by its record id (search focus uses this). */
  selectEdge(id) {
    this.selectedEdge = 'e' + id
    this.selectedNode = null
    this.sigma.refresh()
  }

  /** Highlight a node by its id (click-expand and search focus use this). */
  selectNode(id) {
    this.selectedNode = String(id)
    this.selectedEdge = null
    this.sigma.refresh()
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
    // Low gravity so ForceAtlas2 doesn't pile every disconnected group onto
    // the center (there's no attraction *between* components, only repulsion,
    // so weak gravity lets them drift apart); outbound-attraction spreads hubs.
    forceAtlas2.assign(this.graph, {
      iterations: this.graph.order > 400 ? 60 : 150,
      settings: {
        gravity: 0.4,
        scalingRatio: 18,
        adjustSizes: true,
        outboundAttractionDistribution: true,
        barnesHutOptimize: this.graph.order > 400,
      },
    })
    // Then hard-separate: pack each connected component's bounding box into a
    // grid so unrelated groups never overlap, regardless of how FA2 settled.
    this._packComponents()
    this.sigma.refresh()
  }

  /** Connected components (undirected) as arrays of node keys, via BFS. */
  _components() {
    const seen = new Set()
    const comps = []
    this.graph.forEachNode((start) => {
      if (seen.has(start)) return
      const comp = []
      const stack = [start]
      seen.add(start)
      while (stack.length) {
        const cur = stack.pop()
        comp.push(cur)
        this.graph.forEachNeighbor(cur, (nb) => {
          if (!seen.has(nb)) {
            seen.add(nb)
            stack.push(nb)
          }
        })
      }
      comps.push(comp)
    })
    return comps
  }

  /**
   * Rigidly translate each connected component so their bounding boxes tile a
   * grid with a gap between them — preserving each component's internal FA2
   * layout while guaranteeing unrelated groups don't overlap. No-op for a
   * single component.
   */
  _packComponents(gap = 4) {
    const comps = this._components()
    if (comps.length <= 1) return

    const boxes = comps.map((comp) => {
      let minX = Infinity,
        minY = Infinity,
        maxX = -Infinity,
        maxY = -Infinity
      for (const n of comp) {
        const x = this.graph.getNodeAttribute(n, 'x')
        const y = this.graph.getNodeAttribute(n, 'y')
        if (x < minX) minX = x
        if (y < minY) minY = y
        if (x > maxX) maxX = x
        if (y > maxY) maxY = y
      }
      return { comp, minX, minY, w: maxX - minX || 1, h: maxY - minY || 1 }
    })

    // Shelf-pack tallest-first, wrapping near a square overall aspect.
    boxes.sort((a, b) => b.h - a.h)
    const rowLimit = Math.sqrt(boxes.reduce((s, b) => s + (b.w + gap) * (b.h + gap), 0))
    let cursorX = 0
    let cursorY = 0
    let rowH = 0
    for (const b of boxes) {
      if (cursorX > 0 && cursorX + b.w > rowLimit) {
        cursorX = 0
        cursorY += rowH + gap
        rowH = 0
      }
      const dx = cursorX - b.minX
      const dy = cursorY - b.minY
      for (const n of b.comp) {
        this.graph.setNodeAttribute(n, 'x', this.graph.getNodeAttribute(n, 'x') + dx)
        this.graph.setNodeAttribute(n, 'y', this.graph.getNodeAttribute(n, 'y') + dy)
      }
      cursorX += b.w + gap
      rowH = Math.max(rowH, b.h)
    }
  }

  legendEntries() {
    return [...this.legend.entries()].map(([label, color]) => ({ label, color }))
  }

  destroy() {
    this.sigma.kill()
  }
}
