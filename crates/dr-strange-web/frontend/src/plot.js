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
      // Highlight the edge under the cursor so its clickable region is
      // obvious — thicker, amber, with its type label forced visible.
      edgeReducer: (edge, data) =>
        edge === this.hoveredEdge
          ? { ...data, size: (data.size ?? 4) * 1.8, color: HOVER_COLOR, forceLabel: true, zIndex: 1 }
          : data,
    })

    this.sigma.on('clickNode', ({ node }) =>
      handlers.onSelectNode?.(node, this.graph.getNodeAttribute(node, 'record')),
    )
    this.sigma.on('clickEdge', ({ edge }) =>
      handlers.onSelectEdge?.(edge, this.graph.getEdgeAttribute(edge, 'record')),
    )
    this.sigma.on('clickStage', () => handlers.onClearSelection?.())

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
