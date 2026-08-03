// Tests for the graph reasoning behind the plot's legibility work: which
// neighbours are leaves, which hubs have too many to draw, how leaves are
// arranged, and how far each node is from the focus.
//
// These are the parts that can be wrong in a way a screenshot would not show —
// a leaf mistaken for structure, a fan that draws differently every time, a
// distance measured on a graph that has since changed. The renderer itself
// needs WebGL and is not exercised here.

import { describe, expect, test } from 'bun:test'
import Graph from 'graphology'

import {
  alphaFor,
  dim,
  focusDistances,
  hubsToFold,
  leavesOf,
  sectorLeaves,
  weightByImportance,
} from './layout.js'

/** A hub with `n` leaves, plus `extra` nodes wired to each other and the hub. */
function star(n, { labels = ['Leaf'], extra = 0 } = {}) {
  const g = new Graph({ type: 'directed', multi: true })
  g.addNode('hub', { x: 0, y: 0, record: { labels: ['Hub'] } })
  for (let i = 0; i < n; i++) {
    const label = labels[i % labels.length]
    // Placed on a circle of radius 10 so the mean radius is predictable.
    const a = (2 * Math.PI * i) / n
    g.addNode(`l${i}`, { x: 10 * Math.cos(a), y: 10 * Math.sin(a), record: { labels: [label] } })
    g.addEdgeWithKey(`e${i}`, 'hub', `l${i}`, {})
  }
  for (let j = 0; j < extra; j++) {
    g.addNode(`s${j}`, { x: j, y: -20, record: { labels: ['Struct'] } })
    g.addEdgeWithKey(`se${j}`, 'hub', `s${j}`, {})
  }
  // Wire the structural nodes into a chain so none of them is a leaf.
  for (let j = 1; j < extra; j++) g.addEdgeWithKey(`sc${j}`, `s${j - 1}`, `s${j}`, {})
  return g
}

describe('leavesOf', () => {
  test('a leaf is a neighbour joined to the hub and to nothing else', () => {
    const g = star(3, { extra: 3 })
    expect(leavesOf(g, 'hub').sort()).toEqual(['l0', 'l1', 'l2'])
  })

  test('parallel edges to one hub still make a leaf', () => {
    // Degree would say 2 and call this structure; distinct neighbours say 1.
    const g = star(1)
    g.addEdgeWithKey('dup', 'hub', 'l0', {})
    expect(g.degree('l0')).toBe(2)
    expect(leavesOf(g, 'hub')).toEqual(['l0'])
  })

  test('a node with two neighbours is structure, not a leaf', () => {
    const g = star(2)
    g.addEdgeWithKey('bridge', 'l0', 'l1', {})
    expect(leavesOf(g, 'hub')).toEqual([])
  })

  test('a bead is never a leaf, so a fold is never folded again', () => {
    const g = star(1)
    g.addNode('~bead:hub', { x: 1, y: 1, bead: true, count: 40 })
    g.addEdgeWithKey('~beadedge:hub', 'hub', '~bead:hub', {})
    expect(leavesOf(g, 'hub')).toEqual(['l0'])
  })
})

describe('hubsToFold', () => {
  test('folds a hub past the threshold and leaves a small fan alone', () => {
    expect(hubsToFold(star(30), { min: 20 })).toHaveLength(1)
    expect(hubsToFold(star(19), { min: 20 })).toHaveLength(0)
  })

  test('what the reader is looking at is never folded away', () => {
    const keep = new Set(['l0', 'l1'])
    const [[hub, leaves]] = hubsToFold(star(30), { min: 20, keep })
    expect(hub).toBe('hub')
    expect(leaves).not.toContain('l0')
    expect(leaves).not.toContain('l1')
    expect(leaves).toHaveLength(28)
  })

  test('holding back the focused leaves can spare the whole fan', () => {
    // 21 leaves, two of them focused, leaves 19 — under the threshold. So the
    // fold does not happen at all, and a reader looking at one satellite keeps
    // its siblings on screen rather than watching them vanish around it.
    expect(hubsToFold(star(21), { min: 20, keep: new Set(['l0', 'l1']) })).toHaveLength(0)
  })

  test('a hub the reader opened stays open', () => {
    expect(hubsToFold(star(30), { min: 20, skipHubs: new Set(['hub']) })).toHaveLength(0)
  })

  test('structure is not counted toward the fold', () => {
    // 10 leaves and 15 structural neighbours: a busy hub, but not a fan.
    expect(hubsToFold(star(10, { extra: 15 }), { min: 20 })).toHaveLength(0)
  })
})

describe('sectorLeaves', () => {
  const angleOf = (g, n) =>
    (Math.atan2(g.getNodeAttribute(n, 'y'), g.getNodeAttribute(n, 'x')) + 2 * Math.PI) % (2 * Math.PI)

  test('leaves sharing a label end up adjacent on the ring', () => {
    // Interleaved on input: A, B, A, B, … — so any grouping is the code's work.
    const g = star(12, { labels: ['Alpha', 'Beta'] })
    sectorLeaves(g, 5)
    const byLabel = { Alpha: [], Beta: [] }
    for (const n of leavesOf(g, 'hub')) {
      byLabel[g.getNodeAttribute(n, 'record').labels[0]].push(angleOf(g, n))
    }
    // Each label occupies one contiguous arc: its span is about half the circle,
    // not spread across all of it.
    for (const angles of Object.values(byLabel)) {
      angles.sort((a, b) => a - b)
      const span = angles[angles.length - 1] - angles[0]
      expect(span).toBeLessThan(Math.PI)
    }
  })

  test('the same graph always draws the same way', () => {
    const positions = () => {
      const g = star(10, { labels: ['A', 'B', 'C'] })
      sectorLeaves(g, 5)
      return leavesOf(g, 'hub')
        .sort()
        .map((n) => [n, g.getNodeAttribute(n, 'x').toFixed(6), g.getNodeAttribute(n, 'y').toFixed(6)])
    }
    expect(positions()).toEqual(positions())
  })

  test('a crowded fan is given a wider ring', () => {
    const radiusOf = (n) => {
      const g = star(n)
      sectorLeaves(g, 5)
      const l = leavesOf(g, 'hub')[0]
      return Math.hypot(g.getNodeAttribute(l, 'x'), g.getNodeAttribute(l, 'y'))
    }
    // Both start on a radius-10 circle; the busier one has to grow.
    expect(radiusOf(40)).toBeGreaterThan(radiusOf(6))
  })

  test('only leaves move — structure keeps the layout it was given', () => {
    const g = star(8, { extra: 3 })
    const before = ['s0', 's1', 's2', 'hub'].map((n) => [
      g.getNodeAttribute(n, 'x'),
      g.getNodeAttribute(n, 'y'),
    ])
    sectorLeaves(g, 5)
    const after = ['s0', 's1', 's2', 'hub'].map((n) => [
      g.getNodeAttribute(n, 'x'),
      g.getNodeAttribute(n, 'y'),
    ])
    expect(after).toEqual(before)
  })

  test('a fan below the threshold is left where the layout put it', () => {
    const g = star(4)
    const before = leavesOf(g, 'hub').map((n) => g.getNodeAttribute(n, 'x'))
    sectorLeaves(g, 5)
    expect(leavesOf(g, 'hub').map((n) => g.getNodeAttribute(n, 'x'))).toEqual(before)
  })
})

describe('focusDistances', () => {
  /** A path a—b—c—d—e. */
  function path() {
    const g = new Graph({ type: 'directed', multi: true })
    for (const n of ['a', 'b', 'c', 'd', 'e']) g.addNode(n, { x: 0, y: 0 })
    g.addEdgeWithKey('ab', 'a', 'b', {})
    g.addEdgeWithKey('bc', 'b', 'c', {})
    g.addEdgeWithKey('cd', 'c', 'd', {})
    g.addEdgeWithKey('de', 'd', 'e', {})
    return g
  }

  test('distance is measured outward and stops where told', () => {
    const d = focusDistances(path(), new Set(['a']), 2)
    expect([...d.entries()].sort()).toEqual([
      ['a', 0],
      ['b', 1],
      ['c', 2],
    ])
    // Beyond the ring a node is absent, not far-numbered.
    expect(d.has('d')).toBe(false)
  })

  test('direction does not matter — an edge is a hop either way', () => {
    const d = focusDistances(path(), new Set(['c']), 1)
    expect(d.get('b')).toBe(1)
    expect(d.get('d')).toBe(1)
  })

  test('two focus nodes both start at zero, and the nearer one wins', () => {
    // Selecting an edge focuses both its ends (a and e here).
    const d = focusDistances(path(), new Set(['a', 'e']), 2)
    expect(d.get('a')).toBe(0)
    expect(d.get('e')).toBe(0)
    expect(d.get('b')).toBe(1)
    expect(d.get('d')).toBe(1)
    expect(d.get('c')).toBe(2)
  })

  test('an empty focus measures nothing', () => {
    expect(focusDistances(path(), new Set(), 2).size).toBe(0)
  })

  test('a focus node that has left the graph is ignored', () => {
    const d = focusDistances(path(), new Set(['a', 'gone']), 1)
    expect(d.get('a')).toBe(0)
    expect(d.has('gone')).toBe(false)
  })
})

describe('dim', () => {
  test('a category colour survives fading, so the legend still means something', () => {
    expect(dim('#7c3aed', 0.4)).toBe('rgba(124,58,237,0.4)')
    // Shorthand hex is the same colour written shorter.
    expect(dim('#abc', 0.5)).toBe('rgba(170,187,204,0.5)')
  })

  test('full opacity is left exactly as it was', () => {
    // The reducer runs on every node of every frame; an untouched value here
    // means an untouched object there.
    expect(dim('#7c3aed', 1)).toBe('#7c3aed')
  })

  test('a colour already transparent by intent is not made opaque', () => {
    // The selected node's disc is deliberately invisible so the hover drawer
    // can paint a ring in its place.
    expect(dim('rgba(0,0,0,0)', 0.4)).toBe('rgba(0,0,0,0)')
  })

  test('rgb() gains an alpha channel', () => {
    expect(dim('rgb(1,2,3)', 0.25)).toBe('rgba(1,2,3,0.25)')
  })
})

describe('alphaFor', () => {
  test('the selection and its neighbours stay at full strength', () => {
    expect(alphaFor(0)).toBe(1)
    expect(alphaFor(1)).toBe(1)
  })

  test('the second ring recedes, and everything beyond it recedes further', () => {
    expect(alphaFor(2)).toBeLessThan(1)
    expect(alphaFor(null)).toBeLessThan(alphaFor(2))
    // Never fully invisible: a faded node is context, not an absence.
    expect(alphaFor(null)).toBeGreaterThan(0)
  })
})

describe('weightByImportance', () => {
  /** Two nodes joined by an edge, with the given raw scores. */
  function pair(a, b) {
    const g = new Graph({ type: 'directed', multi: true })
    g.addNode('a', { x: 0, y: 0 })
    g.addNode('b', { x: 1, y: 1 })
    g.addEdgeWithKey('ab', 'a', 'b', {})
    return [g, new Map([['a', a], ['b', b]])]
  }

  test('an edge is weighted by its more important end', () => {
    const [g, scores] = pair(1, 0.1)
    weightByImportance(g, scores, 3)
    // The top score normalises to 1, so the edge carries the full 1 + k.
    expect(g.getEdgeAttribute('ab', 'weight')).toBeCloseTo(4)
  })

  test('scores are normalised, so any scale behaves the same', () => {
    const [big, bigScores] = pair(1000, 100)
    const [small, smallScores] = pair(0.01, 0.001)
    weightByImportance(big, bigScores, 3)
    weightByImportance(small, smallScores, 3)
    expect(big.getEdgeAttribute('ab', 'weight')).toBeCloseTo(
      small.getEdgeAttribute('ab', 'weight'),
    )
  })

  test('an unranked node weighs nothing extra', () => {
    const [g] = pair(0, 0)
    weightByImportance(g, new Map(), 3)
    // No scores at all: every edge stays at the neutral weight.
    expect(g.getEdgeAttribute('ab', 'weight')).toBe(1)
  })

  test('a heavier edge is the LONGER one — the surprise worth pinning', () => {
    // ForceAtlas2 feeds weight into node mass as well as attraction, and the
    // mass term wins, so importance must map to *more* weight to buy room.
    // Measured: a fan of 40 opens from 8.7 to 12.5 node radii at k=3.
    const [g, scores] = pair(1, 0)
    weightByImportance(g, scores, 3)
    expect(g.getEdgeAttribute('ab', 'weight')).toBeGreaterThan(1)
  })
})
