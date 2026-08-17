// WebGL graph plot: a thin imperative wrapper over graphology (data model) +
// sigma (WebGL renderer) + ForceAtlas2 (layout), kept out of Svelte's reactive
// world since these objects mutate in place (arch/08 §2.2). The Explore view
// drives it: seed a subgraph, click a node to expand its neighbourhood.

import Graph from 'graphology'
import Sigma from 'sigma'
import forceAtlas2 from 'graphology-layout-forceatlas2'
import { EdgeCurvedArrowProgram, indexParallelEdgesIndex } from '@sigma/edge-curve'

import {
  alphaFor,
  dim,
  focusDistances,
  frontierIds,
  hubsToFold,
  sectorLeaves,
  weightByImportance,
} from './layout.js'

const DEFAULT_CURVATURE = 0.25 // gentle arc for a solitary edge

// Curvature for the nth of a set of parallel edges (sigma's own formula), so a
// bundle fans out symmetrically instead of overlapping.
function curveFor(index, maxIndex) {
  if (index < 0) return -curveFor(-index, maxIndex)
  const amplitude = 3.5
  const maxCurvature = (amplitude * (1 - Math.exp(-maxIndex / amplitude))) / maxIndex
  return (maxCurvature * index) / maxIndex
}

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
// Algorithm-overlay colours (ROADMAP §1). PATH reuses the amber accent; MUTED
// greys read on both themes so a highlighted route/group pops against them.
const PATH_COLOR = '#f59e0b'
const MUTED_NODE = '#9aa0aa'
const MUTED_EDGE = '#c7ccd4'

// How an edge looks at rest, relative to how it looks under the cursor.
//
// A dense subgraph is mostly edges by area, and at full weight they read as
// the subject of the picture rather than as the relations between the things
// that are. Thinner and more transparent lets the nodes and their labels come
// first while the shape of the graph still reads — this is recession, not
// hiding, so the numbers stay well clear of invisible.
//
// Hover and selection then restore full weight rather than exaggerating it:
// the emphasis is the edge returning to normal, which is a smaller and calmer
// movement than the thickening it replaces.
const EDGE_REST_THICKNESS = 0.6
const EDGE_REST_ALPHA = 0.55
// Connect-drag endpoints: the node the drag started on (source) and the one
// under the cursor (target) get loud, distinct solid colours while dragging.
const CONNECT_SRC = '#16a34a' // green
const CONNECT_DST = '#2563eb' // blue

const BEAD_COLOR = '#8b93a1'
const BEAD_PREFIX = '~bead:'
const BEAD_EDGE_PREFIX = '~beadedge:'

// A theme-aware replacement for sigma's default node-hover drawer. Two jobs:
//  - the SELECTED node (sigma routes `highlighted` nodes here) draws as a
//    hollow ring in its own colour — interior filled with the canvas bg — so it
//    inverts from the solid nodes around it and is unmistakable;
//  - a plain hover draws the label box inverted — text colour as the box, box
//    colour as the text — so the key under the cursor stands out from every
//    other label. (Sigma's own default paints the box hardcoded white, which is
//    invisible under light text in dark mode, so it could not be reused.)
// `label`/`bg`/`canvasBg` are getters read every draw, so theme flips track;
// `graph` gives access to the node's stored (untouched) category colour.
function drawNodeHover(label, bg, canvasBg, graph) {
  return (context, data, settings) => {
    const { labelSize: size, labelFont: font, labelWeight: weight } = settings
    context.font = `${weight} ${size}px ${font}`

    if (data.highlighted) {
      const r = data.size
      // The reducer made the WebGL disc transparent; its category colour still
      // lives on the graph node, which we read for the ring stroke.
      const ringColor = graph.getNodeAttribute(data.key, 'color') || HOVER_COLOR
      context.shadowBlur = 0 // clear any shadow left by a hover box this frame
      context.beginPath()
      context.arc(data.x, data.y, r, 0, Math.PI * 2)
      context.closePath()
      context.fillStyle = canvasBg() // hollow the interior to the background
      context.fill()
      context.lineWidth = Math.max(2.5, r * 0.4)
      context.strokeStyle = ringColor
      context.stroke()
      if (data.label) {
        context.fillStyle = label()
        context.fillText(data.label, data.x + r + 3, data.y + size / 3)
      }
      return
    }

    // Hovered: the label chip is drawn **inverted** — the text colour becomes
    // the box and the box colour becomes the text — so the key under the
    // cursor reads as a solid swatch against a field of plain labels.
    //
    // Inversion rather than a new accent colour, because both values are the
    // ones already chosen for this theme: it is legible on either background
    // by construction, and there is no third colour to keep in step when the
    // palette changes.
    const boxFill = label()
    const textFill = bg()

    context.fillStyle = boxFill
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
      context.fillStyle = textFill
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
    // Nodes drawn as focused. Usually the one selected node — but selecting an
    // edge focuses BOTH its endpoints, because an edge is a statement about two
    // things and looking at it means looking at them.
    this.focusNodes = new Set()
    this.focusDist = null // node -> hops from the focus, or absent when far
    this.collapsed = new Map() // bead key -> { hub, nodes, edges } folded away
    this.expandedHubs = new Set() // hubs the reader opened; never re-folded
    this.scores = new Map() // node id -> importance, when the seed was ranked
    this.hiddenLabels = new Set() // categories switched off from the legend

    // Labels are canvas-drawn, so CSS variables don't reach them — read the
    // OS theme directly and keep them legible on both backgrounds.
    const media = window.matchMedia('(prefers-color-scheme: dark)')
    const nodeLabel = () => (media.matches ? '#e8e8ee' : '#1a1a1e')
    const edgeLabel = () => (media.matches ? '#9ca3af' : '#6b7280')
    // Hover-label box: sigma's default paints it hardcoded white, hiding our
    // light dark-mode label text. Match the panel background per theme instead.
    const hoverBg = () => (media.matches ? '#26262e' : '#ffffff')
    // The plot area's background (--panel), used to hollow the selected-node
    // ring so its interior blends with the canvas.
    const canvasBg = () => (media.matches ? '#1f1f27' : '#ffffff')

    this.sigma = new Sigma(this.graph, container, {
      // Curved edges (arrowheads kept): prettier than straight lines, and a
      // curve per direction keeps A→B and B→A from sitting on top of each other.
      defaultEdgeType: 'curved',
      edgeProgramClasses: { curved: EdgeCurvedArrowProgram },
      enableEdgeEvents: true,
      renderEdgeLabels: true,
      labelColor: { color: nodeLabel() },
      edgeLabelColor: { color: edgeLabel() },
      defaultDrawNodeHover: drawNodeHover(nodeLabel, hoverBg, canvasBg, this.graph),
      labelDensity: 0.5,
      labelRenderedSizeThreshold: 5,
      // Edge picking reads a downsized framebuffer, so a thin edge has no
      // clickable pixels and the hit falls through to the stage. Give edges a
      // floor thickness so they can actually be selected/hovered — this is why
      // the resting weight above is a fraction and not a vanishing one.
      minEdgeThickness: 2.5,
      // Highlight the hovered OR selected edge: full weight, amber, with its
      // type label forced visible. Hover makes the clickable region obvious;
      // the selected edge stays lit so a click/search focus is visible.
      edgeReducer: (edge, data) => {
        if (edge === this.hoveredEdge || edge === this.selectedEdge) {
          return { ...data, size: data.size ?? 4, color: HOVER_COLOR, forceLabel: true, zIndex: 1 }
        }
        if (this.hiddenLabels.size) {
          const src = this.graph.source(edge)
          const dst = this.graph.target(edge)
          const gone = (n) =>
            !this.graph.getNodeAttribute(n, 'bead') && this.hiddenLabels.has(this._categoryOf(n))
          if (gone(src) || gone(dst)) return { ...data, hidden: true }
        }
        // An overlay lit this one on purpose — leave it exactly as it asked.
        if (data.emphasis) return data
        // An edge is only as near as its further end.
        const alpha = this.focusDist
          ? Math.min(
              this._alphaOf(this.graph.source(edge)),
              this._alphaOf(this.graph.target(edge)),
            )
          : 1
        // The resting style, composed with focus fade rather than replaced by
        // it: a far edge is fainter still, and a near one is already receded.
        const rest = {
          ...data,
          size: (data.size ?? 4) * EDGE_REST_THICKNESS,
          color: dim(data.color ?? MUTED_EDGE, alpha * EDGE_REST_ALPHA),
        }
        return alpha >= 1 ? rest : { ...rest, label: '' }
      },
      // The selected node renders as a hollow ring. Sigma draws a highlighted
      // node's solid WebGL disc *over* anything the hover drawer paints, so we
      // make that disc transparent here and let drawNodeHover paint the ring
      // (its colour read from the graph's stored category colour).
      nodeReducer: (node, data) => {
        // Mid-connect: paint the source green and the hovered target blue, both
        // enlarged with forced labels, so it's unmistakable what's being joined.
        if (this.connectFrom) {
          if (node === this.connectFrom) {
            return { ...data, color: CONNECT_SRC, size: (data.size ?? 5) * 1.8, forceLabel: true, zIndex: 2 }
          }
          if (node === this.hoveredNode) {
            return { ...data, color: CONNECT_DST, size: (data.size ?? 5) * 1.8, forceLabel: true, zIndex: 2 }
          }
        }
        if (this.focusNodes.has(node)) {
          return {
            ...data,
            highlighted: true,
            forceLabel: true,
            size: (data.size ?? 5) * 1.35,
            color: 'rgba(0,0,0,0)',
            zIndex: 1,
          }
        }
        if (!data.bead && this.hiddenLabels.size && this.hiddenLabels.has(this._categoryOf(node))) {
          return { ...data, hidden: true }
        }
        // A folded bead always says how much it is hiding.
        if (data.bead) {
          return { ...data, forceLabel: true, color: dim(BEAD_COLOR, this._alphaOf(node)) }
        }
        const alpha = this._alphaOf(node)
        // Labels are dropped rather than faded past the focus ring: unreadable
        // text still costs the space it occupies.
        return alpha >= 1 ? data : { ...data, color: dim(data.color, alpha), label: alpha > 0.2 ? data.label : '' }
      },
    })

    this.sigma.on('clickNode', ({ node }) => {
      // A bead is not a thing in the graph — clicking it opens what it folded.
      if (this.graph.getNodeAttribute(node, 'bead')) {
        this.expandBead(node)
        return
      }
      this.selectedNode = node
      this.selectedEdge = null // a node click supersedes any edge selection
      this._setFocus([node])
      handlers.onSelectNode?.(node, this.graph.getNodeAttribute(node, 'record'))
    })
    this.sigma.on('clickEdge', ({ edge }) => {
      this.selectedEdge = edge
      this.selectedNode = null
      this._setFocus([this.graph.source(edge), this.graph.target(edge)])
      handlers.onSelectEdge?.(edge, this.graph.getEdgeAttribute(edge, 'record'))
    })
    this.sigma.on('clickStage', () => {
      this.selectedEdge = null
      this.selectedNode = null
      this._setFocus([])
      handlers.onClearSelection?.()
    })

    // Recolour labels if the OS theme flips while the plot is open.
    media.addEventListener('change', () => {
      this.sigma.setSetting('labelColor', { color: nodeLabel() })
      this.sigma.setSetting('edgeLabelColor', { color: edgeLabel() })
    })

    // Pointer cursor + edge highlight on hover. `hoveredNode` is the connect
    // target when a left-drag from another node is released.
    this.sigma.on('enterNode', ({ node }) => {
      this.hoveredNode = node
      this._cursor(this.connectFrom ? 'crosshair' : 'pointer')
      if (this.connectFrom) this.sigma.refresh() // repaint the target highlight
    })
    this.sigma.on('leaveNode', () => {
      this.hoveredNode = null
      this._cursor(this.connectFrom ? 'crosshair' : '')
      if (this.connectFrom) this.sigma.refresh()
    })
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

    this._setupMouse(handlers)
  }

  // Custom mouse model (arch/08): the RIGHT button drags to pan the canvas
  // (sigma only pans on left); the LEFT button never pans — instead a left-drag
  // starting on a node and released on another opens an edge between them.
  _setupMouse(handlers) {
    this.rightPan = null // { lastX, lastY } while right-dragging
    this.leftDown = false
    this.hoveredNode = null
    this.connectFrom = null // node a left-drag started on

    const rel = (e) => {
      const r = this.container.getBoundingClientRect()
      return { x: e.clientX - r.left, y: e.clientY - r.top }
    }

    // Right-drag pans; suppress its context menu.
    this.container.addEventListener('contextmenu', (e) => e.preventDefault())
    this._onDown = (e) => {
      if (e.button === 2) {
        const p = rel(e)
        this.rightPan = { lastX: p.x, lastY: p.y }
        e.preventDefault()
      } else if (e.button === 0) {
        this.leftDown = true
      }
    }
    this.container.addEventListener('mousedown', this._onDown)

    // A left-drag starting on a node begins a "connect" gesture.
    this.sigma.on('downNode', ({ node }) => {
      this.connectFrom = node
      this._cursor('crosshair')
      this.sigma.refresh() // light up the source
    })

    // Block sigma's built-in left-drag camera pan (we pan with the right button).
    this.sigma.getMouseCaptor().on('mousemovebody', (e) => {
      if (this.leftDown) e.preventSigmaDefault()
    })

    // Right-drag pan, replicating sigma's own viewport→graph delta math.
    this._onMove = (e) => {
      if (!this.rightPan) return
      const p = rel(e)
      const camera = this.sigma.getCamera()
      const last = this.sigma.viewportToFramedGraph({ x: this.rightPan.lastX, y: this.rightPan.lastY })
      const cur = this.sigma.viewportToFramedGraph({ x: p.x, y: p.y })
      const s = camera.getState()
      camera.setState({ x: s.x + (last.x - cur.x), y: s.y + (last.y - cur.y) })
      this.rightPan.lastX = p.x
      this.rightPan.lastY = p.y
    }
    window.addEventListener('mousemove', this._onMove)

    this._onUp = () => {
      // Finish a connect gesture: left-drag from one node, released on another.
      if (!this.rightPan && this.connectFrom && this.hoveredNode && this.hoveredNode !== this.connectFrom) {
        const src = this.graph.getNodeAttribute(this.connectFrom, 'record')
        const dst = this.graph.getNodeAttribute(this.hoveredNode, 'record')
        // A bead stands for many nodes and is none of them; there is no edge to
        // draw to it.
        if (src && dst) handlers.onConnect?.(src, dst)
      }
      this.connectFrom = null
      this.leftDown = false
      this.rightPan = null
      this._cursor(this.hoveredNode ? 'pointer' : '')
      this.sigma.refresh() // clear the source/target highlights
    }
    window.addEventListener('mouseup', this._onUp)
  }

  /** Whether a node id is currently on the canvas. */
  hasNode(id) {
    return this.graph.hasNode(String(id))
  }

  /** How many real entities are held, counting those folded into beads. */
  entityCount() {
    let n = 0
    this.graph.forEachNode((_node, attrs) => {
      if (!attrs.bead) n += 1
    })
    for (const { nodes } of this.collapsed.values()) n += nodes.length
    return n
  }

  /**
   * The frontier: entities with at most one edge *on the canvas*.
   *
   * These are where the picture stops rather than where the graph does — a node
   * shown with one connection is almost always one whose other neighbours were
   * never fetched. Expanding exactly these grows the view outward by a ring,
   * instead of re-fetching the well-connected interior already drawn.
   *
   * **Folded leaves count.** `_collapseLeaves` drops a hub's leaves from the
   * graph and leaves a bead in their place, so after any seed most of the
   * frontier is inside beads rather than in the graph. A bead is how those
   * nodes are *drawn*; it says nothing about whether their neighbours have been
   * fetched, and skipping them leaves the frontier looking empty on exactly the
   * views that have one.
   */
  leafIds() {
    return frontierIds(this.graph, this.collapsed)
  }

  /**
   * Add a single edge between two nodes already on the canvas and refresh —
   * NO relayout, so connecting two visible nodes leaves the graph put.
   */
  addEdgeInPlace(e) {
    const key = 'e' + e.id
    if (this.graph.hasEdge(key)) return
    if (this.graph.hasNode(String(e.src)) && this.graph.hasNode(String(e.dst))) {
      this.graph.addEdgeWithKey(key, String(e.src), String(e.dst), {
        label: e.type,
        size: 4,
        record: e,
      })
      this._curveEdges() // re-fan in case this created a parallel bundle
      this._sizeByDegree() // both endpoints just gained a degree
      this.sigma.refresh()
    }
  }

  _cursor(value) {
    this.container.style.cursor = value
  }

  clear() {
    this.graph.clear()
    this.legend.clear()
    this.selectedEdge = null
    this.selectedNode = null
    this.focusNodes = new Set()
    this.focusDist = null
    this.collapsed.clear()
    this.expandedHubs.clear()
    this.scores.clear()
  }

  /**
   * Highlight an edge by its record id (search focus uses this), focusing both
   * of its endpoints with it.
   */
  selectEdge(id) {
    const key = 'e' + id
    this.selectedEdge = key
    this.selectedNode = null
    this._setFocus(this.graph.hasEdge(key) ? [this.graph.source(key), this.graph.target(key)] : [])
  }

  /** Highlight a node by its id (click-expand and search focus use this). */
  selectNode(id) {
    this.selectedNode = String(id)
    this.selectedEdge = null
    this._setFocus([String(id)])
  }

  /**
   * Set which nodes read as focused, and measure the graph outward from them so
   * everything else can recede. Distance is what makes a dense canvas legible:
   * a selection with its neighbourhood at full strength and the rest faded is
   * the same picture with the noise turned down.
   */
  _setFocus(nodes) {
    this.focusNodes = new Set(nodes.filter((n) => this.graph.hasNode(n)))
    if (!this.focusNodes.size) {
      this.focusDist = null
      this.sigma.refresh()
      return
    }
    this.focusDist = focusDistances(this.graph, this.focusNodes)
    this.sigma.refresh()
  }

  /** Opacity for a node at its hop distance from the focus. 1 when unfocused. */
  _alphaOf(node) {
    return this.focusDist ? alphaFor(this.focusDist.get(node)) : 1
  }

  /**
   * Merge a `{nodes, edges}` subgraph in. New nodes are seeded near `anchor`
   * (the clicked node) so an expansion grows outward instead of teleporting;
   * existing nodes keep their positions. Returns nothing — call sites read
   * `legendEntries()` afterward.
   */
  addSubgraph({ nodes = [], edges = [], scores = null }, anchorId = null) {
    for (const { id, score } of scores ?? []) this.scores.set(String(id), score)
    const anchor =
      anchorId != null && this.graph.hasNode(String(anchorId))
        ? {
            x: this.graph.getNodeAttribute(String(anchorId), 'x'),
            y: this.graph.getNodeAttribute(String(anchorId), 'y'),
          }
        : { x: 0, y: 0 }

    // Unfold every bead before merging. A newly-arrived edge may well point at
    // a node that is currently folded away, and an edge whose endpoint is not
    // on the canvas is silently dropped — so the graph is made whole first and
    // re-folded afterwards, rather than reasoning about what each bead hides.
    this._restoreAllBeads()

    this._addNodes(nodes, anchor)
    this._addEdges(edges)

    this._curveEdges()
    this._collapseLeaves()
    this._sizeByDegree()
    this._layout()
  }

  _addNodes(nodes, anchor) {
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
  }

  _addEdges(edges) {
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
  }

  /**
   * Fold each hub's leaves into a single bead once there are too many to read.
   * Nothing is lost: the records ride on the bead and come back when it is
   * opened, or whenever the canvas next changes.
   */
  _collapseLeaves() {
    const work = hubsToFold(this.graph, {
      skipHubs: this.expandedHubs,
      keep: this.focusNodes,
    })

    for (const [hub, leaves] of work) {
      const nodes = []
      const edges = []
      for (const leaf of leaves) {
        nodes.push(this.graph.getNodeAttribute(leaf, 'record'))
        this.graph.forEachEdge(leaf, (_e, attrs) => edges.push(attrs.record))
        this.graph.dropNode(leaf) // takes its edges with it
      }
      const key = BEAD_PREFIX + hub
      this.collapsed.set(key, { hub, nodes, edges })
      this.graph.addNode(key, {
        label: `+${leaves.length}`,
        x: this.graph.getNodeAttribute(hub, 'x') + 1,
        y: this.graph.getNodeAttribute(hub, 'y') + 1,
        size: 6,
        color: BEAD_COLOR,
        bead: true,
        count: leaves.length,
      })
      this.graph.addEdgeWithKey(BEAD_EDGE_PREFIX + hub, hub, key, {
        label: `${leaves.length} more`,
        size: 4,
      })
    }
  }

  /** Put every folded leaf back, dropping the beads. */
  _restoreAllBeads() {
    if (!this.collapsed.size) return
    for (const [key, { nodes, edges }] of this.collapsed) {
      const anchor = this.graph.hasNode(key)
        ? { x: this.graph.getNodeAttribute(key, 'x'), y: this.graph.getNodeAttribute(key, 'y') }
        : { x: 0, y: 0 }
      if (this.graph.hasNode(key)) this.graph.dropNode(key)
      this._addNodes(nodes, anchor)
      this._addEdges(edges)
    }
    this.collapsed.clear()
  }

  /** Open one bead, and remember not to fold that hub again. */
  expandBead(key) {
    const folded = this.collapsed.get(key)
    if (!folded) return
    this.expandedHubs.add(folded.hub)
    const anchor = { x: this.graph.getNodeAttribute(key, 'x'), y: this.graph.getNodeAttribute(key, 'y') }
    this.graph.dropNode(key)
    this.collapsed.delete(key)
    this._addNodes(folded.nodes, anchor)
    this._addEdges(folded.edges)
    this._curveEdges()
    this._sizeByDegree()
    this._layout()
  }

  /**
   * Assign each edge a curvature: a gentle default for a lone edge, and a
   * fanned-out value for members of a parallel bundle so they don't overlap.
   */
  _curveEdges() {
    indexParallelEdgesIndex(this.graph, {
      edgeIndexAttribute: 'parallelIndex',
      edgeMinIndexAttribute: 'parallelMinIndex',
      edgeMaxIndexAttribute: 'parallelMaxIndex',
    })
    this.graph.forEachEdge((edge, attrs) => {
      const { parallelIndex, parallelMaxIndex } = attrs
      const curvature =
        typeof parallelIndex === 'number' && parallelMaxIndex
          ? curveFor(parallelIndex, parallelMaxIndex)
          : DEFAULT_CURVATURE
      this.graph.setEdgeAttribute(edge, 'curvature', curvature)
    })
  }

  /**
   * Radius by degree, so the core node in a fan reads as the core node without
   * an overlay switched on. A hub counts the leaves folded into its bead — its
   * importance did not shrink when they were tidied away — and a bead counts
   * what it hides.
   */
  _sizeByDegree() {
    const folded = new Map() // hub -> leaves hidden beneath it
    for (const { hub, nodes } of this.collapsed.values()) {
      folded.set(hub, (folded.get(hub) ?? 0) + nodes.length)
    }
    this.graph.forEachNode((node, attrs) => {
      if (attrs.bead) {
        this.graph.setNodeAttribute(node, 'size', 6 + Math.min(16, Math.sqrt(attrs.count) * 2.2))
        return
      }
      const hidden = folded.get(node) ?? 0
      // The bead edge stands in for the leaves it replaced; don't count both.
      const d = this.graph.degree(node) + (hidden ? hidden - 1 : 0)
      this.graph.setNodeAttribute(node, 'size', 4 + Math.min(14, Math.sqrt(d) * 2.5))
    })
  }

  _layout() {
    if (this.graph.order === 0) {
      this.sigma.refresh()
      return
    }
    if (this.scores.size) weightByImportance(this.graph, this.scores)
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
    // Then arrange each hub's leaves into label sectors, and only then pack:
    // sectoring moves nodes, and packing measures bounding boxes, so doing it
    // the other way round would let a re-arranged fan spill into its neighbour.
    sectorLeaves(this.graph)
    this._packComponents()
    // Distances were measured on the graph as it was; folding, unfolding and
    // expanding all change what is a hop away.
    this._setFocus([...this.focusNodes])
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

  /**
   * Switch whole categories off. Hiding rather than fading, because a reader
   * turning off a label has said it is not part of the question — leaving a
   * ghost of it behind would be answering a question they did not ask.
   */
  setHiddenLabels(labels) {
    this.hiddenLabels = new Set(labels)
    this.sigma.refresh()
  }

  /** The category a node belongs to, as the legend names it. */
  _categoryOf(node) {
    return this.graph.getNodeAttribute(node, 'record')?.labels?.[0] ?? '(none)'
  }

  legendEntries() {
    return [...this.legend.entries()].map(([label, color]) => ({ label, color }))
  }

  // ---- algorithm overlays (ROADMAP §1) ------------------------------------
  // These decorate whatever nodes are currently on the canvas from a
  // `plane.algo` result (keyed by String(id)); `resetStyle` undoes them.

  /**
   * Scale each on-canvas node's radius by its score (PageRank importance),
   * normalized across the visible nodes so the biggest is always readable.
   * Nodes absent from the map keep the minimum size.
   */
  overlayScores(scoreOf, { min = 4, max = 26 } = {}) {
    let lo = Infinity
    let hi = -Infinity
    this.graph.forEachNode((n) => {
      const s = scoreOf.get(n)
      if (s == null) return
      if (s < lo) lo = s
      if (s > hi) hi = s
    })
    const span = hi - lo || 1
    this.graph.forEachNode((n) => {
      const s = scoreOf.get(n)
      const size = s == null ? min : min + (max - min) * Math.sqrt((s - lo) / span)
      this.graph.setNodeAttribute(n, 'size', size)
    })
    this.sigma.refresh()
  }

  /**
   * Recolor on-canvas nodes by group (component / community). Groups are
   * numbered in first-seen order for a stable, readable legend; nodes with no
   * group are muted. Returns `[{label, color}]` for the groups shown.
   */
  overlayGroups(groupOf, prefix = 'group') {
    const order = new Map() // rawGroup -> { color, idx }
    this.graph.forEachNode((node) => {
      const g = groupOf.get(node)
      if (g == null) {
        this.graph.setNodeAttribute(node, 'color', MUTED_NODE)
        return
      }
      if (!order.has(g)) order.set(g, { color: PALETTE[order.size % PALETTE.length], idx: order.size })
      this.graph.setNodeAttribute(node, 'color', order.get(g).color)
    })
    this.sigma.refresh()
    return [...order.values()]
      .sort((a, b) => a.idx - b.idx)
      .map(({ color, idx }) => ({ label: `${prefix} ${idx + 1}`, color }))
  }

  /**
   * Emphasize a path: its nodes/edges glow in the accent colour (endpoints
   * enlarged), everything else is muted so the route stands out. `nodeIds` /
   * `edgeIds` are the raw ids from a `shortest_path` result.
   */
  highlightPath(nodeIds, edgeIds) {
    const np = new Set(nodeIds.map(String))
    const ep = new Set(edgeIds.map((id) => 'e' + id))
    this.graph.forEachNode((n) => {
      const on = np.has(n)
      this.graph.setNodeAttribute(n, 'color', on ? PATH_COLOR : MUTED_NODE)
      this.graph.setNodeAttribute(n, 'size', on ? 11 : 4)
    })
    this.graph.forEachEdge((e) => {
      const on = ep.has(e)
      this.graph.setEdgeAttribute(e, 'color', on ? PATH_COLOR : MUTED_EDGE)
      this.graph.setEdgeAttribute(e, 'size', on ? 6 : 2)
      // Exempt from the resting recession: an edge on the route is the thing
      // the reader asked to see, so it keeps the weight this overlay gave it.
      // The rest still recede, which is the point of the overlay.
      this.graph.setEdgeAttribute(e, 'emphasis', on)
    })
    this.sigma.refresh()
  }

  /** Undo any overlay: restore category colours + degree sizing. */
  resetStyle() {
    this.graph.forEachNode((n, attrs) => {
      // Beads are not entities: giving one a category colour would also invent
      // a legend entry for the label it does not have.
      if (attrs.bead) {
        this.graph.setNodeAttribute(n, 'color', BEAD_COLOR)
        return
      }
      const label = attrs.record?.labels?.[0] ?? ''
      this.graph.setNodeAttribute(n, 'color', colorFor(label, this.legend))
    })
    this.graph.forEachEdge((e) => {
      this.graph.removeEdgeAttribute(e, 'color')
      this.graph.removeEdgeAttribute(e, 'emphasis')
      this.graph.setEdgeAttribute(e, 'size', 4)
    })
    this._sizeByDegree()
    this.sigma.refresh()
  }

  destroy() {
    window.removeEventListener('mousemove', this._onMove)
    window.removeEventListener('mouseup', this._onUp)
    this.sigma.kill()
  }
}
