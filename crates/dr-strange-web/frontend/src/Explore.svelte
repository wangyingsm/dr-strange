<script>
  import { onMount, onDestroy } from 'svelte'
  import { rpc, authHeaders, liveChanges } from './rpc.js'
  import { loadPref, savePref } from './prefs.js'
  import { Plot } from './plot.js'
  import CreatePlane from './CreatePlane.svelte'
  import Icon from './Icon.svelte'

  // Providers with an embedding endpoint (deepseek is chat-only, so excluded) —
  // used to embed a text `SEARCH … NEAR "…"`.
  const EMBED_PROVIDERS = ['openai', 'qwen', 'ollama']
  // Chat providers for NL→plan (`plane.ask`); deepseek is chat-capable.
  const CHAT_PROVIDERS = ['openai', 'deepseek', 'qwen', 'ollama']

  // `plane` is the app-wide current plane (App owns it); `focus` is a
  // { id, nonce } signal from the header search — center that node.
  // `onPlaneCreated` bubbles a new plane up to App (refresh picker + switch).
  // `asOf` is the app-wide time-travel cursor (App owns it so the header search
  // and this plot stay in sync); `history` / `timeTravel` come from App's
  // `plane.history` probe. `asOf` is bindable so this view's slider can drive it.
  let {
    plane,
    focus,
    onPlaneCreated = () => {},
    asOf = $bindable(null),
    history = null,
    timeTravel = false,
  } = $props()

  let newPlaneOpen = $state(false) // new-plane popup open?
  let tab = $state('filters') // active toolbar tab: filters | graphql | algorithms | hybrid

  let container // canvas div (bind:this)
  let plot = null
  let started = false // plot created + first seed done
  let seededPlane = null // last plane we seeded (avoids re-seeding on no-op)
  let appliedFocus = -1 // last focus nonce we centered (idempotent)

  let labels = $state([]) // catalog label names for the filter
  let catLabels = $state({}) // full catalog: label -> { properties: { name -> { types } } }
  let labelFilter = $state('') // '' = all labels
  let cypher = $state('') // query-language text; '' = use the label seed
  let embedProvider = $state(loadPref('embedProvider', 'openai')) // text SEARCH … NEAR provider
  let selected = $state(null) // { kind: 'node'|'edge', data }
  let legend = $state([])
  let status = $state('')
  let error = $state(null)
  let vectorView = $state(null) // { k, values } — floats popup, null = closed

  // Algorithm overlays (ROADMAP §1): decorate the plotted graph with a
  // `plane.algo` result. `algoLegend` (when set) supersedes the category legend.
  let algoBusy = $state(false)
  let algoLegend = $state([]) // [{ label, color }] for a group overlay
  let spFrom = $state('') // shortest-path source (id or @key)
  let spTo = $state('') // shortest-path target
  let spDir = $state(loadPref('spDir', 'out')) // out | in | both

  // Hybrid retrieval (ROADMAP §2): pick a node type first, then the channels it
  // supports. "all labels" (*) searches the whole plane semantically — keyword
  // needs a specific label's index. A channel is only offered when it can run.
  let hyQuery = $state('') // the query text
  let hyLabel = $state('') // '*' = all labels, else a specific label
  let hyIndexes = $state(null) // { vector:[{label,property,metric}], keyword:[{label,property}] } | null
  let useVector = $state(true) // want the semantic channel? (shown only when available)
  let useKeyword = $state(true) // want the keyword channel? (shown only when available)
  let hyGraph = $state(false) // add the 1-hop graph-proximity channel
  let hyProvider = $state(loadPref('hyProvider', 'openai')) // embedding provider for the semantic channel
  let hyResults = $state(null) // ranked hits | null

  // A label's embedding property from the catalog (a Vector-typed prop). Semantic
  // is offered whenever one exists — a declared vector index only accelerates it;
  // vector search brute-forces over the property without one.
  function catVectorProp(label) {
    const props = catLabels?.[label]?.properties ?? {}
    return Object.keys(props).find((k) => props[k]?.types && 'Vector' in props[k].types) ?? null
  }
  // A plane-wide embedding property for "all labels" (prefers "embedding").
  function anyVectorProp() {
    const names = new Set()
    for (const l of Object.keys(catLabels ?? {})) {
      const p = catVectorProp(l)
      if (p) names.add(p)
    }
    return names.has('embedding') ? 'embedding' : ([...names][0] ?? null)
  }

  let isAll = $derived(hyLabel === '*')
  // "all labels" is only offered when some label actually has embeddings.
  let hasAll = $derived(anyVectorProp() != null)
  // Specific labels supporting a channel: an embedding prop (semantic) or a
  // declared keyword index (keyword).
  let searchableLabels = $derived.by(() => {
    const set = new Set()
    for (const x of hyIndexes?.keyword ?? []) set.add(x.label)
    for (const l of Object.keys(catLabels ?? {})) if (catVectorProp(l)) set.add(l)
    return [...set].sort()
  })

  // Per-channel property for the current selection (null = unavailable). Semantic
  // prefers a declared index (for its metric); keyword needs a specific label's
  // declared index, so it's never available for "all".
  let vecIndex = $derived(isAll ? null : (hyIndexes?.vector.find((x) => x.label === hyLabel) ?? null))
  let vecMetric = $derived(vecIndex?.metric ?? 'cosine')
  let semanticProp = $derived(isAll ? anyVectorProp() : (vecIndex?.property ?? catVectorProp(hyLabel)))
  let keywordProp = $derived(
    isAll ? null : (hyIndexes?.keyword.find((x) => x.label === hyLabel)?.property ?? null),
  )

  // Effective channels (checked AND available); the query box + Search appear
  // only once at least one is active.
  let vectorOn = $derived(useVector && semanticProp != null)
  let keywordOn = $derived(useKeyword && keywordProp != null)
  let anyChannel = $derived(vectorOn || keywordOn)

  // NL→plan (ROADMAP §3): ask a question, an LLM turns it into a read-only plan.
  let askQuestion = $state('')
  let askProvider = $state(loadPref('askProvider', 'openai')) // chat provider (key from the server env)
  let askEmbed = $state(loadPref('askEmbed', 'openai')) // embed provider for find_edge/find_entity ('' = tools off)
  let askDryRun = $state(false) // return the plan without running it
  let askResult = $state(null) // { plans, ran, attempts, results, count } | null
  let copied = $state(false) // "copied" flash on the plan copy button

  // "Declare an index" dialog (so the dashboard never sends you to the CLI).
  let idxOpen = $state(false)
  let idxKind = $state(loadPref('idxKind', 'keyword')) // keyword | vector
  let idxLabel = $state('')
  let idxProperty = $state('')
  let idxMetric = $state(loadPref('idxMetric', 'cosine'))
  let idxError = $state(null)

  // ---- time-travel / AS OF (ROADMAP §4) -----------------------------------
  // The cursor (`asOf`) and window (`history`) are owned by App; this view only
  // drives the slider and re-plots. `sliderSeq` is the slider's own position.
  let sliderSeq = $state(0) // bound slider position (equals latest when live)
  let sliderInit = false // seeded the slider from the window yet?
  let lastAsOf = null // last cursor value we re-plotted at (dedup guard)

  // The AS OF params to fold into a graph read; empty when viewing live.
  const atParams = () => (asOf == null ? {} : { as_of: asOf })

  // Position the slider once the window is known (don't fight an active drag,
  // which moves `sliderSeq` while `asOf` stays put until release).
  $effect(() => {
    if (history && !sliderInit) {
      sliderInit = true
      sliderSeq = asOf ?? history.latest
    }
  })

  // Re-plot whenever the cursor actually changes — whether from this view's
  // slider/Live button or App's header chip. Skips the drag (which only moves
  // `sliderSeq`) and the no-op initial run.
  $effect(() => {
    const cur = asOf
    if (cur === lastAsOf) return
    lastAsOf = cur
    if (history) sliderSeq = cur ?? history.latest
    if (started) seed()
  })

  // Snap back to the live (latest) view; the effect above re-plots.
  function goLive() {
    asOf = null
  }

  // ---- live change feed (ROADMAP §5) --------------------------------------
  // A WebSocket `plane.watch` subscription streams commits as they land. The
  // feed is a capped, newest-first list; clicking an entry focuses that node.
  const FEED_CAP = 200
  let feed = $state([]) // [{ seq, kind, op, id, labels, record }], newest first
  let feedLabel = $state('') // '' = all labels, else narrow the subscription
  let feedPaused = $state(false) // stop streaming without leaving the tab
  let feedLive = $state(false) // WebSocket connected?

  function onChange(params) {
    // Flatten the commit's changes newest-first, tag each with the commit seq.
    const incoming = (params.changes ?? []).map((c) => ({ ...c, seq: params.seq }))
    if (incoming.length) feed = [...incoming.reverse(), ...feed].slice(0, FEED_CAP)
  }

  // Manage the subscription's lifecycle: connect while the Live tab is active,
  // for the current plane + label filter, and not paused. Re-subscribes (fresh
  // feed) when any of those change; disconnects on leave/unmount.
  $effect(() => {
    if (tab !== 'live' || !plane || feedPaused) return
    const label = feedLabel // capture the filter for this subscription
    feed = []
    feedLive = false
    const dispose = liveChanges(plane, label || null, onChange, (open) => (feedLive = open))
    return () => {
      dispose()
      feedLive = false
    }
  })

  function opClass(op) {
    return op === 'created' ? 'op-created' : op === 'deleted' ? 'op-deleted' : 'op-updated'
  }

  // Commit the slider to `asOf`; `latest` ⇒ live. The effect above re-plots.
  function applyTimeTravel() {
    if (!history) return
    asOf = sliderSeq >= history.latest ? null : sliderSeq
  }

  // Remember dropdown selections across reloads.
  $effect(() => {
    savePref('embedProvider', embedProvider)
    savePref('spDir', spDir)
    savePref('hyProvider', hyProvider)
    savePref('askProvider', askProvider)
    savePref('askEmbed', askEmbed)
    savePref('idxKind', idxKind)
    savePref('idxMetric', idxMetric)
  })
  // Candidate properties for the chosen label + kind: string props for keyword,
  // vector props for semantic (from the catalog's observed types). For the
  // "all labels" target, the union of matching-typed property names.
  let idxProps = $derived.by(() => {
    const want = idxKind === 'vector' ? 'Vector' : 'Str'
    const hasType = (p) => p?.types && want in p.types
    if (idxLabel === '*') {
      const names = new Set()
      for (const l of Object.keys(catLabels ?? {})) {
        const props = catLabels[l]?.properties ?? {}
        for (const k of Object.keys(props)) if (hasType(props[k])) names.add(k)
      }
      return [...names].sort()
    }
    const props = catLabels?.[idxLabel]?.properties ?? {}
    return Object.keys(props).filter((k) => hasType(props[k]))
  })

  // Inspector mutation state (mutation UI): edit properties + delete.
  let editing = $state(false)
  let draft = $state([]) // editable [{ key, value }] rows of scalar props
  let newKey = $state('')
  let newValue = $state('')
  let saveError = $state(null)
  let draftLabels = $state('') // node: comma-separated labels being edited
  let draftType = $state('') // edge: type being edited

  // Inspector delete (type-to-confirm popup).
  let confirmingDelete = $state(false)
  let deleteInput = $state('')
  let deleteError = $state(null)

  // Create-node / create-edge state.
  let creating = $state(null) // null | 'node' | 'edge'
  let createError = $state(null)
  let nKey = $state('')
  let nLabels = $state('')
  let eSrc = $state('')
  let eDst = $state('')
  let eType = $state('')

  // Flatten a properties object (values are raw, `{ $desc, $value }`, or an
  // embedding `{ $vector: [...] }`), then sink underscore-prefixed
  // provenance/internal props to the bottom — each group keeps its order.
  function propEntries(props) {
    const entries = Object.entries(props ?? {}).map(([k, raw]) => {
      let v = raw
      let desc = null
      if (v && typeof v === 'object' && '$value' in v) {
        desc = v.$desc
        v = v.$value
      }
      if (v && typeof v === 'object' && Array.isArray(v.$vector)) {
        v = v.$vector // unwrap embeddings so they collapse to a button
      }
      return { k, v, desc }
    })
    const inner = entries.filter((e) => e.k.startsWith('_'))
    const outer = entries.filter((e) => !e.k.startsWith('_'))
    return [...outer, ...inner]
  }

  // A numeric array = an embedding vector; render a button, not 128+ floats.
  const isVector = (v) => Array.isArray(v) && v.length > 0 && v.every((x) => typeof x === 'number')

  // Pretty grid: fixed-width columns of 6 values, index-addressable via rows.
  function formatVector(v) {
    const cols = 6
    const rows = []
    for (let i = 0; i < v.length; i += cols) {
      rows.push(v.slice(i, i + cols).map((x) => x.toFixed(5).padStart(10)).join(' '))
    }
    return rows.join('\n')
  }

  async function loadCatalog() {
    try {
      const cat = await rpc('plane.catalog', { plane })
      labels = Object.keys(cat.labels ?? {})
      catLabels = cat.labels ?? {}
    } catch {
      labels = []
      catLabels = {}
    }
  }

  async function seed() {
    error = null
    try {
      plot.clear()
      const params = { plane, ...atParams() }
      if (labelFilter) params.label = labelFilter
      const sg = await rpc('graph.seed', params)
      plot.addSubgraph(sg)
      legend = plot.legendEntries()
      selected = null
      status = `${sg.nodes.length} nodes · ${sg.edges.length} edges${
        sg.truncated ? ` (of ${sg.total}, capped)` : ''
      }`
    } catch (e) {
      error = e.message
    }
  }

  // Keyword autocomplete for the GraphQL/Cypher box: once the word being typed
  // is >2 chars and prefixes a keyword, `cypherGhost` is the greyed completion
  // shown after the caret; Tab accepts it.
  const CYPHER_KEYWORDS = [
    'MATCH', 'WHERE', 'RETURN', 'LIMIT', 'ORDER BY', 'SKIP', 'CREATE', 'MERGE',
    'SET', 'DELETE', 'REMOVE', 'DETACH', 'WITH', 'SEARCH', 'NEAR', 'TOPK',
    'DISTINCT', 'AND', 'OR', 'NOT', 'AS', 'ON',
  ]
  let cypherGhost = $derived.by(() => {
    const m = cypher.match(/([A-Za-z]+)$/) // the word currently being typed
    if (!m || m[1].length < 3) return ''
    const up = m[1].toUpperCase()
    const kw = CYPHER_KEYWORDS.find((k) => k.startsWith(up) && k.length > up.length)
    return kw ? kw.slice(m[1].length) : ''
  })
  function onCypherKey(e) {
    if (e.key === 'Enter') {
      runCypher()
    } else if (e.key === 'Tab' && cypherGhost) {
      e.preventDefault()
      cypher = cypher + cypherGhost + ' '
    }
  }

  // Run a query-language statement against the current plane. A read renders
  // its result (nodes + induced edges) as a fresh subgraph; a write (CREATE, …)
  // mutates and reports counts, then reloads the canvas. Empty query → fall
  // back to the plain label seed. Hits the web-only POST /cypher endpoint (not
  // an RPC method), so it uses a raw fetch with the bearer token.
  async function runCypher() {
    if (!cypher.trim()) {
      await seed()
      return
    }
    error = null
    algoBusy = true
    try {
      const url = `/cypher?plane=${encodeURIComponent(plane)}&embed=${encodeURIComponent(embedProvider)}`
      const res = await fetch(url, {
        method: 'POST',
        headers: authHeaders({ 'content-type': 'text/plain' }),
        body: cypher,
      })
      if (!res.ok) throw new Error((await res.text()) || `query failed (${res.status})`)
      const out = await res.json()
      if (out.write) {
        // A write mutated the plane — summarise the non-zero counts, then reseed
        // so the canvas reflects the new graph.
        const bits = [
          [out.nodes_created, 'nodes created'],
          [out.edges_created, 'edges created'],
          [out.props_set, 'props set'],
          [out.labels_set, 'labels set'],
          [out.nodes_deleted, 'nodes deleted'],
          [out.edges_deleted, 'edges deleted'],
        ]
          .filter(([n]) => n > 0)
          .map(([n, label]) => `${n} ${label}`)
        status = bits.length ? bits.join(' · ') : 'no changes'
        await seed()
        return
      }
      plot.clear()
      plot.addSubgraph(out)
      legend = plot.legendEntries()
      selected = null
      status = `${out.count} nodes · ${out.edges.length} edges`
    } catch (e) {
      error = e.message
    } finally {
      algoBusy = false
    }
  }

  async function expand(id) {
    try {
      const sg = await rpc('graph.expand', { plane, id, direction: 'both', ...atParams() })
      plot.addSubgraph(sg, id)
      legend = plot.legendEntries()
      status = `expanded +${sg.nodes.length} nodes${
        sg.truncated ? ` (${sg.total - sg.nodes.length} more not shown)` : ''
      }`
    } catch (e) {
      error = e.message
    }
  }

  // ---- algorithm overlays (ROADMAP §1) ------------------------------------

  // A high cap so the overlay covers every visible node (the RPC returns a
  // ranked/grouped list capped at `limit`; whole-plane runs can be large).
  const ALGO_CAP = 1_000_000

  // Resolve a "id or @key" reference to a numeric node id (for shortest path).
  async function resolveRef(s) {
    const t = s.trim()
    if (!t) return null
    if (/^\d+$/.test(t)) return Number(t)
    const key = t.startsWith('@') ? t.slice(1) : t
    const node = await rpc('node.get', { plane, key })
    return node?.id ?? null
  }

  async function runPagerank() {
    algoBusy = true
    error = null
    try {
      const res = await rpc('plane.algo', { plane, algo: 'pagerank', limit: ALGO_CAP })
      const scoreOf = new Map(res.results.map((r) => [String(r.id), r.score]))
      plot.overlayScores(scoreOf)
      algoLegend = []
      const top = res.results[0]
      status = top
        ? `PageRank · ${res.count} nodes · top #${top.id} (${top.score.toFixed(4)})`
        : `PageRank · no nodes`
    } catch (e) {
      error = e.message
    } finally {
      algoBusy = false
    }
  }

  // Louvain communities or connected components — both recolour by group.
  async function runGroups(algo) {
    algoBusy = true
    error = null
    const isComm = algo === 'louvain'
    const key = isComm ? 'community' : 'component'
    const prefix = isComm ? 'community' : 'component'
    try {
      const res = await rpc('plane.algo', { plane, algo, limit: ALGO_CAP })
      const groupOf = new Map(res.results.map((r) => [String(r.id), r[key]]))
      algoLegend = plot.overlayGroups(groupOf, prefix)
      status = `${isComm ? 'Communities' : 'Components'} · ${res.count} groups over ${res.results.length} nodes`
    } catch (e) {
      error = e.message
    } finally {
      algoBusy = false
    }
  }

  async function runShortestPath() {
    algoBusy = true
    error = null
    try {
      const [src, dst] = await Promise.all([resolveRef(spFrom), resolveRef(spTo)])
      if (src == null || dst == null) {
        error = 'enter two known nodes (id or @key) for the path'
        return
      }
      const res = await rpc('plane.algo', { plane, algo: 'shortest_path', src, dst, dir: spDir })
      if (!res.found) {
        status = `no ${spDir} path from ${spFrom} to ${spTo}`
        return
      }
      // Bring each hop's neighbourhood onto the canvas so the whole route (and
      // its edges) is present, then light up the path and mute the rest.
      for (const id of res.path.nodes) {
        plot.addSubgraph(await rpc('graph.expand', { plane, id, direction: 'both', ...atParams() }), id)
      }
      plot.highlightPath(res.path.nodes, res.path.edges)
      algoLegend = []
      legend = plot.legendEntries()
      status = `path · ${res.path.nodes.length} nodes · ${res.path.edges.length} hops · cost ${res.path.cost}`
    } catch (e) {
      error = e.message
    } finally {
      algoBusy = false
    }
  }

  // Clear any overlay/result and re-plot the plane's original graph (respecting
  // the current label filter). Shared "Reset" for every tab.
  async function resetView() {
    algoLegend = []
    hyResults = null
    askResult = null
    selected = null
    await seed()
  }

  // ---- hybrid retrieval (ROADMAP §2) --------------------------------------

  // Load the catalog (for embedding-property detection) + the declared indexes,
  // then pick a searchable label. Channel toggles stay a user preference (both
  // default on); the bar only *shows* a channel when the label supports it.
  async function loadIndexes() {
    await loadCatalog()
    try {
      hyIndexes = await rpc('plane.indexes', { plane })
    } catch {
      hyIndexes = { vector: [], keyword: [] }
    }
    const opts = [...(hasAll ? ['*'] : []), ...searchableLabels]
    if (!opts.includes(hyLabel)) hyLabel = opts[0] ?? ''
  }

  function openIndexDialog() {
    idxError = null
    idxKind = 'keyword'
    idxLabel = hyLabel || labels[0] || ''
    idxProperty = ''
    idxMetric = 'cosine'
    idxOpen = true
  }

  async function declareIndex() {
    idxError = null
    const property = idxProperty.trim()
    if (!idxLabel || !property) {
      idxError = 'pick a label and a property'
      return
    }
    try {
      const want = idxKind === 'vector' ? 'Vector' : 'Str'
      const hasType = (p) => p?.types && want in p.types
      // "All labels" expands to every label that actually has a matching-typed
      // property of that name, so we never declare an index on a label without it.
      let targets
      if (idxLabel === '*') {
        targets = Object.keys(catLabels ?? {}).filter((l) =>
          hasType(catLabels[l]?.properties?.[property]),
        )
        if (!targets.length) {
          idxError = `no label has a ${idxKind === 'vector' ? 'vector' : 'string'} "${property}" property`
          return
        }
      } else {
        targets = [idxLabel]
      }
      for (const l of targets) {
        const params = { plane, label: l, property, kind: idxKind }
        if (idxKind === 'vector') params.metric = idxMetric
        await rpc('index.ensure', params)
      }
      idxOpen = false
      await loadIndexes() // the new indexes now appear
      if (idxLabel !== '*') hyLabel = idxLabel // select it → its channels light up
      status =
        idxLabel === '*'
          ? `${idxKind} index declared on "${property}" across ${targets.length} labels`
          : `${idxKind} index declared on ${idxLabel}.${property}`
    } catch (e) {
      idxError = e.message
    }
  }

  async function runHybrid() {
    if (!hyQuery.trim() || !anyChannel) return
    algoBusy = true
    error = null
    try {
      const params = { plane, q: hyQuery.trim(), k: 25 }
      if (!isAll) params.label = hyLabel // "all" ⇒ whole-plane semantic
      if (vectorOn) {
        params.vector_prop = semanticProp
        params.metric = vecMetric
        params.provider = hyProvider
      }
      if (keywordOn) params.keyword_prop = keywordProp
      if (hyGraph) params.graph_hops = 1
      const res = await rpc('plane.hybrid', params)
      hyResults = res.results
      askResult = null
      await plotResultNodes(res.results)
      const top = res.results[0]
      status = res.count
        ? `hybrid: ${res.count} results · top ${top.external_key ?? '#' + top.id}`
        : 'hybrid: no results'
    } catch (e) {
      error = e.message
    } finally {
      algoBusy = false
    }
  }

  // Plot a result set: the hit nodes plus the edges induced among them (for
  // context), sized by score where present (hybrid) or uniformly (ask).
  async function plotResultNodes(results) {
    plot.clear()
    plot.addSubgraph({ nodes: results, edges: [] })
    const inSet = new Set(results.map((r) => String(r.id)))
    for (const r of results) {
      const sg = await rpc('graph.expand', { plane, id: r.id, direction: 'both' })
      const edges = sg.edges.filter((e) => inSet.has(String(e.src)) && inSet.has(String(e.dst)))
      if (edges.length) plot.addSubgraph({ nodes: [], edges })
    }
    plot.overlayScores(new Map(results.map((r) => [String(r.id), r.score])))
    algoLegend = []
    legend = plot.legendEntries()
  }

  // Format a channel's raw contribution for the results list (— when absent).
  const fmtCh = (v) => (v == null ? '—' : v.toFixed(2))

  // ---- NL→plan ask (ROADMAP §3) -------------------------------------------

  async function runAsk() {
    if (!askQuestion.trim()) return
    algoBusy = true
    error = null
    try {
      const params = {
        plane,
        question: askQuestion.trim(),
        provider: askProvider,
        dry_run: askDryRun,
      }
      if (askEmbed) params.embed_provider = askEmbed // enable the grounding tools
      const res = await rpc('plane.ask', params)
      askResult = res
      hyResults = null
      if (res.ran) {
        // Plot the matched subgraph directly — its nodes AND the edges among
        // them (source + traversal) — so the answer is a connected graph.
        plot.clear()
        plot.addSubgraph({ nodes: res.results ?? [], edges: res.edges ?? [] })
        legend = plot.legendEntries()
        algoLegend = []
      }
      const tries = `${res.attempts} attempt${res.attempts === 1 ? '' : 's'}`
      status = res.ran
        ? `ask: ${res.results.length} nodes · ${res.edges?.length ?? 0} edges · ${tries}`
        : `ask: plan generated (dry run) · ${tries}`
    } catch (e) {
      error = e.message
    } finally {
      algoBusy = false
    }
  }

  // Copy the generated plan JSON to the clipboard (brief "copied" flash).
  let copyTimer
  async function copyPlans() {
    if (!askResult?.plans) return
    try {
      await navigator.clipboard.writeText(JSON.stringify(askResult.plans, null, 2))
      copied = true
      clearTimeout(copyTimer)
      copyTimer = setTimeout(() => (copied = false), 1200)
    } catch {
      error = 'clipboard copy failed'
    }
  }

  // ---- inspector mutations ------------------------------------------------

  // Only plain scalar, non-provenance props are editable — vectors (embeddings)
  // and underscore-prefixed provenance stay read-only.
  function editableEntries(props) {
    return propEntries(props).filter((e) => !e.k.startsWith('_') && !isVector(e.v))
  }

  // Parse an input the way the backend will: valid JSON (42, true, "x") keeps
  // its type; anything else is a plain string.
  function parseValue(s) {
    try {
      return JSON.parse(s)
    } catch {
      return s
    }
  }

  function startEdit() {
    saveError = null
    draft = editableEntries(selected.data.properties).map((e) => ({
      key: e.k,
      value: typeof e.v === 'string' ? e.v : JSON.stringify(e.v),
    }))
    newKey = ''
    newValue = ''
    draftLabels = selected.kind === 'node' ? (selected.data.labels ?? []).join(', ') : ''
    draftType = selected.kind === 'edge' ? (selected.data.type ?? '') : ''
    editing = true
  }

  function cancelEdit() {
    editing = false
    draft = []
    saveError = null
  }

  function removeDraftRow(i) {
    draft = draft.filter((_, j) => j !== i)
  }

  async function saveEdit() {
    saveError = null
    const rows = [...draft]
    if (newKey.trim()) rows.push({ key: newKey.trim(), value: newValue })

    // `set` = every kept row; `unset` = editable keys that were removed.
    const set = {}
    for (const r of rows) if (r.key.trim()) set[r.key.trim()] = parseValue(r.value)
    const kept = new Set(rows.map((r) => r.key.trim()))
    const unset = editableEntries(selected.data.properties)
      .map((e) => e.k)
      .filter((k) => !kept.has(k))

    try {
      let updated
      if (selected.kind === 'node') {
        const labels = draftLabels
          .split(',')
          .map((s) => s.trim())
          .filter(Boolean)
        updated = await rpc('node.update', { plane, id: selected.data.id, set, unset, labels })
      } else {
        const params = { plane, edge: selected.data.id, set, unset }
        if (draftType.trim()) params.type = draftType.trim()
        updated = await rpc('edge.update', params)
      }
      selected = { kind: selected.kind, data: updated }
      editing = false
      status = `updated ${selected.kind} ${updated.id}`
    } catch (e) {
      saveError = e.message
    }
  }

  // The token the user must type to confirm a delete: a node's external key
  // (or its id when keyless), an edge's id.
  function deleteToken() {
    if (!selected) return ''
    return selected.kind === 'node'
      ? (selected.data.external_key ?? String(selected.data.id))
      : String(selected.data.id)
  }

  function askDelete() {
    deleteInput = ''
    deleteError = null
    confirmingDelete = true
  }
  function cancelDelete() {
    confirmingDelete = false
  }

  async function confirmDelete() {
    if (!selected || deleteInput.trim() !== deleteToken()) return // type-to-confirm
    const kind = selected.kind
    const token = deleteToken()
    deleteError = null
    try {
      if (kind === 'node') await rpc('node.delete', { plane, id: selected.data.id })
      else await rpc('edge.delete', { plane, edge: selected.data.id })
      confirmingDelete = false
      selected = null
      editing = false
      await seed() // reload the canvas without the deleted element
      status = `deleted ${kind} ${token}`
    } catch (e) {
      deleteError = e.message
    }
  }

  function resetCreate() {
    creating = null
    createError = null
    nKey = ''
    nLabels = ''
    eSrc = ''
    eDst = ''
    eType = ''
  }

  function openCreate(kind) {
    resetCreate()
    creating = kind
  }

  function autofocus(el) {
    el.focus()
  }

  function onKeydown(e) {
    if (e.key !== 'Escape') return
    if (creating) resetCreate()
    else if (confirmingDelete) cancelDelete()
    else if (idxOpen) idxOpen = false
  }

  async function createNode() {
    createError = null
    const labels = nLabels
      .split(',')
      .map((s) => s.trim())
      .filter(Boolean)
    const params = { plane, labels }
    if (nKey.trim()) params.key = nKey.trim()
    try {
      const node = await rpc('node.create', params)
      resetCreate()
      await focusNode(node.id) // center + select the new node (then Edit for props)
    } catch (e) {
      createError = e.message
    }
  }

  // A numeric string is a node id; anything else is an external key (matches
  // the backend's NodeRef).
  const nodeRef = (s) => (/^\d+$/.test(s.trim()) ? Number(s.trim()) : s.trim())

  async function createEdge() {
    createError = null
    if (!eType.trim()) {
      createError = 'edge type is required'
      return
    }
    try {
      const edge = await rpc('edge.create', {
        plane,
        src: nodeRef(eSrc),
        dst: nodeRef(eDst),
        type: eType.trim(),
      })
      resetCreate()
      // If both endpoints are already on the canvas (the common case for a
      // drag-connect), just drop the edge in — no relayout, graph stays put.
      if (plot.hasNode(edge.src) && plot.hasNode(edge.dst)) {
        plot.addEdgeInPlace(edge)
        plot.selectEdge(edge.id)
        selected = { kind: 'edge', data: edge }
        status = `created edge ${edge.id}`
      } else {
        await focusEdge(edge) // an endpoint isn't shown — center on the new edge
      }
    } catch (e) {
      createError = e.message
    }
  }

  // Leaving a selection (or switching to a new one) drops any in-progress edit.
  $effect(() => {
    selected // track
    editing = false
  })

  // Center a specific node (from header search): show it plus its 1-hop
  // neighborhood and select it.
  async function focusNode(id) {
    error = null
    try {
      const node = await rpc('node.get', { plane, id })
      if (!node) {
        status = `node ${id} not found`
        return
      }
      plot.clear()
      plot.addSubgraph({ nodes: [node], edges: [] })
      const sg = await rpc('graph.expand', { plane, id, direction: 'both', ...atParams() })
      plot.addSubgraph(sg, id)
      plot.selectNode(id) // keep the focused node lit
      legend = plot.legendEntries()
      selected = { kind: 'node', data: node }
      status = `focused ${node.external_key ?? `#${id}`} · +${sg.nodes.length} neighbors`
    } catch (e) {
      error = e.message
    }
  }

  // Center an edge (from header search): show both endpoints + their
  // neighborhoods and select the edge itself.
  async function focusEdge(edge) {
    error = null
    try {
      const [src, dst] = await Promise.all([
        rpc('node.get', { plane, id: edge.src }),
        rpc('node.get', { plane, id: edge.dst }),
      ])
      plot.clear()
      plot.addSubgraph({ nodes: [src, dst].filter(Boolean), edges: [edge] })
      for (const id of [edge.src, edge.dst]) {
        plot.addSubgraph(await rpc('graph.expand', { plane, id, direction: 'both', ...atParams() }), id)
      }
      plot.selectEdge(edge.id) // keep the found edge lit
      legend = plot.legendEntries()
      selected = { kind: 'edge', data: edge }
      status = `focused ${edge.type}: ${src?.external_key ?? `#${edge.src}`} → ${dst?.external_key ?? `#${edge.dst}`}`
    } catch (e) {
      error = e.message
    }
  }

  async function applyFocus() {
    if (focus.kind === 'edge') await focusEdge(focus.edge)
    else await focusNode(focus.id)
  }

  // Re-seed when the current plane changes (order-independent: whoever runs
  // first — onMount or the effect below — seeds once, the other no-ops).
  async function reseed() {
    if (!started || plane === seededPlane) return
    seededPlane = plane
    labelFilter = ''
    // The catalog (labels + embedding-prop detection) is loaded by loadIndexes,
    // which runs on the same plane-change signal — no need to fetch it twice.
    await seed()
  }

  // Apply a pending focus signal once (idempotent by nonce).
  async function maybeFocus() {
    if (!started || !focus || focus.nonce === appliedFocus) return
    appliedFocus = focus.nonce
    await applyFocus()
  }

  onMount(async () => {
    plot = new Plot(container, {
      onSelectNode: (_node, record) => {
        selected = { kind: 'node', data: record }
        expand(record.id)
      },
      onSelectEdge: (_edge, record) => {
        selected = { kind: 'edge', data: record }
      },
      onClearSelection: () => {
        selected = null
      },
      // Left-drag from one node to another: open the New Edge popup with the
      // endpoints prefilled (the user just supplies the edge type).
      onConnect: (src, dst) => {
        resetCreate()
        eSrc = src.external_key ?? String(src.id)
        eDst = dst.external_key ?? String(dst.id)
        creating = 'edge'
      },
    })
    started = true
    if (focus) {
      // Arrived here from a search hit — go straight to that node/edge's
      // neighborhood instead of seeding (then discarding) the whole plane.
      seededPlane = plane
      appliedFocus = focus.nonce
      await applyFocus()
    } else {
      await reseed()
    }
  })

  // React to plane switches and search focuses after mount.
  $effect(() => {
    plane // track
    reseed()
  })
  // Refresh the searchable indexes whenever the plane changes (independent of
  // the seed/focus flow, so it runs on the search-focus mount path too).
  $effect(() => {
    plane // track
    loadIndexes()
  })
  $effect(() => {
    focus // track
    maybeFocus()
  })

  onDestroy(() => plot?.destroy())
</script>

<svelte:window onkeydown={onKeydown} />

<div class="tool-tabs">
  <button class:active={tab === 'filters'} onclick={() => (tab = 'filters')}><Icon name="filters" /> Filters / Operations</button>
  <button class:active={tab === 'graphql'} onclick={() => (tab = 'graphql')}><Icon name="graphql" /> GraphQL / Run</button>
  <button class:active={tab === 'algorithms'} onclick={() => (tab = 'algorithms')}><Icon name="algorithms" /> Algorithms</button>
  <button class:active={tab === 'hybrid'} onclick={() => (tab = 'hybrid')}><Icon name="hybrid" /> Hybrid</button>
  <button class:active={tab === 'ask'} onclick={() => (tab = 'ask')}><Icon name="ask" /> Ask</button>
  {#if timeTravel}
    <button class:active={tab === 'timetravel'} class:travelling={asOf != null} onclick={() => (tab = 'timetravel')}>
      <Icon name="timetravel" /> Time-travel{#if asOf != null}<span class="tt-dot" aria-hidden="true"></span>{/if}
    </button>
  {/if}
  <button class:active={tab === 'live'} onclick={() => (tab = 'live')}>
    <Icon name="live" /> Live{#if tab === 'live' && feedLive}<span class="live-dot" aria-hidden="true"></span>{/if}
  </button>
</div>

{#if tab === 'filters'}
  <div class="controls">
    <label>
      Label
      <select bind:value={labelFilter} onchange={seed}>
        <option value="">all</option>
        {#each labels as l (l)}
          <option value={l}>{l}</option>
        {/each}
      </select>
    </label>
    <button onclick={seed}>Reload</button>
    <button class="new-node-btn" onclick={() => openCreate('node')} title="Create a node">New Node</button>
    <button class="new-edge-btn" onclick={() => openCreate('edge')} title="Create an edge">New Edge</button>
    <button class="new-plane-btn" onclick={() => (newPlaneOpen = true)} title="Create a new plane">New Plane</button>
  </div>
{:else if tab === 'graphql'}
  <div class="query-bar">
    <div class="cypher-wrap">
      {#if cypherGhost}
        <div class="cypher-ghost"><span class="typed">{cypher}</span>{cypherGhost}<span class="tab-key">Tab</span></div>
      {/if}
      <input
        type="text"
        class="cypher"
        placeholder={'MATCH (n:Label) WHERE n.p > 1 RETURN n LIMIT 50   ·   SEARCH (n:Label) ON embedding NEAR "some text" TOPK 10 RETURN n'}
        bind:value={cypher}
        onkeydown={onCypherKey}
      />
    </div>
    <label class="embed-pick" title={'Embedding provider for a text SEARCH … NEAR "…" (must match how the plane was embedded)'}>
      embed
      <select bind:value={embedProvider}>
        {#each EMBED_PROVIDERS as pv (pv)}<option value={pv}>{pv}</option>{/each}
      </select>
    </label>
    <button class="run-btn" onclick={runCypher} title="Run this query and plot the result">Run</button>
    <button class="ghost" onclick={resetView} disabled={algoBusy} title="Clear results and re-plot the original graph">Reset</button>
  </div>
{:else if tab === 'algorithms'}
  <div class="algo-bar">
    <button onclick={runPagerank} disabled={algoBusy} title="Size nodes by PageRank importance">PageRank</button>
    <button onclick={() => runGroups('louvain')} disabled={algoBusy} title="Colour nodes by Louvain community">Communities</button>
    <button onclick={() => runGroups('components')} disabled={algoBusy} title="Colour nodes by connected component">Components</button>
    <span class="algo-sep"></span>
    <span class="algo-sp-label">Path</span>
    <input class="sp" placeholder="from (id/@key)" bind:value={spFrom} onkeydown={(e) => e.key === 'Enter' && runShortestPath()} />
    <input class="sp" placeholder="to (id/@key)" bind:value={spTo} onkeydown={(e) => e.key === 'Enter' && runShortestPath()} />
    <select bind:value={spDir} title="Edge direction to follow">
      <option value="out">→ out</option>
      <option value="in">← in</option>
      <option value="both">↔ both</option>
    </select>
    <button onclick={runShortestPath} disabled={algoBusy} title="Shortest path between the two nodes">Find path</button>
    <span class="algo-sep"></span>
    <button class="ghost" onclick={resetView} disabled={algoBusy} title="Clear overlays and re-plot the original graph">Reset</button>
  </div>
{:else if tab === 'hybrid'}
  <div class="algo-bar hybrid-bar">
    {#if hasAll || searchableLabels.length}
    <span class="algo-sp-label">in</span>
    <select bind:value={hyLabel} title="Node type to search">
      {#if hasAll}<option value="*">all labels</option>{/if}
      {#each searchableLabels as l (l)}<option value={l}>{l}</option>{/each}
    </select>
    <span class="algo-sep"></span>
    {#if semanticProp}
      <label class="hy-graph" title="Semantic (vector) similarity">
        <input type="checkbox" bind:checked={useVector} /> semantic
      </label>
    {/if}
    {#if keywordProp}
      <label class="hy-graph" title="BM25 keyword match on {keywordProp}">
        <input type="checkbox" bind:checked={useKeyword} /> keyword
      </label>
    {/if}
    <label class="hy-graph" title="Boost neighbours of the strongest hits (1 hop)">
      <input type="checkbox" bind:checked={hyGraph} /> graph
    </label>
    {#if anyChannel}
      <span class="algo-sep"></span>
      <input
        class="hy-q"
        placeholder="query text…"
        bind:value={hyQuery}
        onkeydown={(e) => e.key === 'Enter' && runHybrid()}
      />
      {#if vectorOn}
        <span class="algo-sp-label">embed</span>
        <select bind:value={hyProvider} title="Embedding provider for the semantic channel">
          {#each EMBED_PROVIDERS as p (p)}<option value={p}>{p}</option>{/each}
        </select>
      {/if}
      <button onclick={runHybrid} disabled={algoBusy || !hyQuery.trim()} title="Run hybrid retrieval and rank by fused score">Search</button>
    {/if}
    <span class="algo-sep"></span>
    <button class="ghost" onclick={openIndexDialog} title="Declare a new search index">＋ index</button>
    {:else}
      <span class="hy-hint">No search index on this plane yet.</span>
      <button class="ghost" onclick={openIndexDialog}>＋ Declare an index</button>
    {/if}
    <span class="algo-sep"></span>
    <button class="ghost" onclick={resetView} disabled={algoBusy} title="Clear results and re-plot the original graph">Reset</button>
  </div>
{:else if tab === 'ask'}
  <div class="algo-bar hybrid-bar">
    <input
      class="hy-q ask-q"
      placeholder="ask a question in plain language…"
      bind:value={askQuestion}
      onkeydown={(e) => e.key === 'Enter' && runAsk()}
    />
    <span class="algo-sp-label">chat</span>
    <select bind:value={askProvider} title="Chat provider for NL→plan (key from the server env)">
      {#each CHAT_PROVIDERS as p (p)}<option value={p}>{p}</option>{/each}
    </select>
    <span class="algo-sp-label">ground</span>
    <select bind:value={askEmbed} title="Embedding provider for the find_edge/find_entity grounding tools (should match how the plane was embedded; off = schema only)">
      <option value="">off</option>
      {#each EMBED_PROVIDERS as p (p)}<option value={p}>{p}</option>{/each}
    </select>
    <label class="hy-graph" title="Generate the plan without running it">
      <input type="checkbox" bind:checked={askDryRun} /> plan only
    </label>
    <button onclick={runAsk} disabled={algoBusy || !askQuestion.trim()} title="Turn the question into a read-only plan and run it">Ask</button>
    <span class="algo-sep"></span>
    <button class="ghost" onclick={resetView} disabled={algoBusy} title="Clear the result and re-plot the original graph">Reset</button>
  </div>
{:else if tab === 'timetravel'}
  <div class="algo-bar tt-bar">
    {#if history && history.latest > history.oldest}
      <span class="algo-sp-label">as of</span>
      <input
        class="tt-slider"
        type="range"
        min={history.oldest}
        max={history.latest}
        step="1"
        bind:value={sliderSeq}
        onchange={applyTimeTravel}
        title="Drag back through commit history"
      />
      <span class="tt-readout" class:live={asOf == null}>
        {#if asOf == null}
          Live · commit {history.latest}
        {:else}
          commit {sliderSeq} / {history.latest} · {history.latest - sliderSeq} back
        {/if}
      </span>
      <span class="algo-sep"></span>
      <button class="ghost" onclick={goLive} disabled={asOf == null} title="Jump back to the latest state">Live</button>
    {:else}
      <span class="tt-empty">Only one commit so far — make more changes to travel back through history.</span>
    {/if}
  </div>
{:else if tab === 'live'}
  <div class="algo-bar live-bar">
    <button
      class:active={!feedPaused}
      onclick={() => (feedPaused = !feedPaused)}
      title={feedPaused ? 'Resume streaming changes' : 'Stop streaming changes'}
    >
      {feedPaused ? 'Resume' : 'Pause'}
    </button>
    <span class="algo-sp-label">label</span>
    <select bind:value={feedLabel} title="Only stream changes to nodes of this label">
      <option value="">all</option>
      {#each labels as l (l)}<option value={l}>{l}</option>{/each}
    </select>
    <span class="live-status" class:on={feedLive && !feedPaused}>
      {#if feedPaused}paused{:else if feedLive}watching “{plane}”{:else}connecting…{/if}
      · {feed.length} change{feed.length === 1 ? '' : 's'}
    </span>
    <span class="algo-sep"></span>
    <button class="ghost" onclick={() => (feed = [])} disabled={!feed.length} title="Clear the feed">Clear</button>
  </div>
{/if}

<CreatePlane bind:open={newPlaneOpen} onCreated={onPlaneCreated} />

{#if creating === 'node'}
  <div class="dlg-backdrop">
    <div class="dlg" role="dialog" aria-modal="true" aria-label="Create a node">
      <header>
        New node
        <button class="close" onclick={resetCreate} aria-label="Close">×</button>
      </header>
      <div class="dlg-body">
        <input placeholder="key (optional)" bind:value={nKey} use:autofocus onkeydown={(e) => e.key === 'Enter' && createNode()} />
        <input placeholder="labels (comma-separated)" bind:value={nLabels} onkeydown={(e) => e.key === 'Enter' && createNode()} />
        {#if createError}<p class="dlg-error">{createError}</p>{/if}
        <div class="dlg-actions">
          <button class="ghost" onclick={resetCreate}>Cancel</button>
          <button class="primary" onclick={createNode}>Create node</button>
        </div>
      </div>
    </div>
  </div>
{:else if creating === 'edge'}
  <div class="dlg-backdrop">
    <div class="dlg" role="dialog" aria-modal="true" aria-label="Create an edge">
      <header>
        New edge
        <button class="close" onclick={resetCreate} aria-label="Close">×</button>
      </header>
      <div class="dlg-body">
        <input placeholder="src (key or id)" bind:value={eSrc} use:autofocus onkeydown={(e) => e.key === 'Enter' && createEdge()} />
        <input placeholder="dst (key or id)" bind:value={eDst} onkeydown={(e) => e.key === 'Enter' && createEdge()} />
        <input placeholder="type" bind:value={eType} onkeydown={(e) => e.key === 'Enter' && createEdge()} />
        {#if createError}<p class="dlg-error">{createError}</p>{/if}
        <div class="dlg-actions">
          <button class="ghost" onclick={resetCreate}>Cancel</button>
          <button class="primary" onclick={createEdge}>Create edge</button>
        </div>
      </div>
    </div>
  </div>
{/if}

{#if idxOpen}
  <div class="dlg-backdrop">
    <div class="dlg" role="dialog" aria-modal="true" aria-label="Declare an index">
      <header>
        New index
        <button class="close" onclick={() => (idxOpen = false)} aria-label="Close">×</button>
      </header>
      <div class="dlg-body">
        <label class="idx-row">
          <span>kind</span>
          <select bind:value={idxKind}>
            <option value="keyword">keyword — BM25 text</option>
            <option value="vector">semantic — embedding</option>
          </select>
        </label>
        <label class="idx-row">
          <span>label</span>
          <select bind:value={idxLabel}>
            <option value="*">✱ all labels</option>
            {#each labels as l (l)}<option value={l}>{l}</option>{/each}
          </select>
        </label>
        <input
          list="idx-props"
          placeholder="property"
          bind:value={idxProperty}
          use:autofocus
          onkeydown={(e) => e.key === 'Enter' && declareIndex()}
        />
        <datalist id="idx-props">
          {#each idxProps as pr (pr)}<option value={pr}></option>{/each}
        </datalist>
        {#if idxKind === 'vector'}
          <label class="idx-row">
            <span>metric</span>
            <select bind:value={idxMetric}>
              <option value="cosine">cosine</option>
              <option value="dot">dot</option>
              <option value="l2">l2</option>
            </select>
          </label>
        {/if}
        <p class="idx-note">
          Indexes {idxKind === 'vector' ? 'embedding (vector)' : 'string'} values of the chosen
          property.{' '}
          {#if idxLabel === '*'}
            Declares it on every label that has such a property (e.g.
            <code>description</code> across all types).
          {:else}
            The suggestions list properties of that type on the label.
          {/if}
        </p>
        {#if idxError}<p class="dlg-error">{idxError}</p>{/if}
        <div class="dlg-actions">
          <button class="ghost" onclick={() => (idxOpen = false)}>Cancel</button>
          <button class="primary" onclick={declareIndex}>Create index</button>
        </div>
      </div>
    </div>
  </div>
{/if}

{#if error}<p class="error">{error}</p>{/if}

<div class="canvas-wrap">
  <div class="canvas" bind:this={container}></div>

  {#if asOf != null}
    <button class="tt-banner" onclick={goLive} title="Viewing a past snapshot — click to return to live">
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <path d="M3 3v5h5" />
        <path d="M3.05 13A9 9 0 1 0 6 5.3L3 8" />
      </svg>
      as of commit {asOf}{#if history} of {history.latest}{/if} · back to live
    </button>
  {/if}

  {#if algoBusy}
    <div class="thinking-overlay plot-thinking">
      <div class="thinking-box">
        <svg class="portal" viewBox="0 0 64 64" fill="none" stroke="#d9a441" stroke-width="2" stroke-linejoin="round" aria-hidden="true">
          <g class="cw">
            <circle cx="32" cy="32" r="30" />
            <circle cx="32" cy="32" r="25.5" stroke-width="3" stroke-dasharray="1.2 3" />
          </g>
          <g class="ccw">
            <rect x="16" y="16" width="32" height="32" />
            <rect x="16" y="16" width="32" height="32" transform="rotate(45 32 32)" />
            <circle cx="32" cy="32" r="11" />
          </g>
          <circle class="core" cx="32" cy="32" r="3.5" fill="#d9a441" stroke="none" />
        </svg>
        <p>working…</p>
      </div>
    </div>
  {/if}

  {#if status}
    <div class="plot-status">{status}</div>
  {/if}

  {#if hyResults && tab === 'hybrid'}
    <div class="hy-results">
      <header>
        <span>Hybrid · {hyResults.length}</span>
        <button class="close" onclick={() => (hyResults = null)} aria-label="Close">×</button>
      </header>
      <ol>
        {#each hyResults as r, i (r.id)}
          <li>
            <button onclick={() => focusNode(r.id)} title="Focus this node">
              <span class="rank">{i + 1}</span>
              <span class="k">{r.external_key ?? `#${r.id}`}</span>
              <span class="sc">{r.score.toFixed(3)}</span>
              <span class="ch" title="vector / keyword / graph contributions">
                v {fmtCh(r.channels?.vector)} · k {fmtCh(r.channels?.keyword)} · g {fmtCh(r.channels?.graph)}
              </span>
            </button>
          </li>
        {/each}
      </ol>
    </div>
  {/if}

  {#if askResult && tab === 'ask'}
    <div class="hy-results ask-plan">
      <header>
        <span>
          Plan · {askResult.attempts} attempt{askResult.attempts === 1 ? '' : 's'}
          {#if !askResult.ran} · dry run{:else} · {askResult.count} results{/if}
        </span>
        <span class="hy-head-actions">
          <button class="copy" onclick={copyPlans} title="Copy the plan JSON">{copied ? 'copied' : 'copy'}</button>
          <button class="close" onclick={() => (askResult = null)} aria-label="Close">×</button>
        </span>
      </header>
      <pre>{JSON.stringify(askResult.plans, null, 2)}</pre>
    </div>
  {/if}

  {#if tab === 'live'}
    <div class="hy-results live-feed">
      <header>
        <span class="live-head" class:on={feedLive && !feedPaused}>
          <span class="live-dot" aria-hidden="true"></span>
          Live changes · {feed.length}
        </span>
        <button class="close" onclick={() => (feed = [])} aria-label="Clear" title="Clear">×</button>
      </header>
      {#if feed.length}
        <ol>
          {#each feed as c, i (`${c.seq}:${c.kind}:${c.id}:${i}`)}
            <li>
              <button
                onclick={() => c.kind === 'node' && c.op !== 'deleted' && focusNode(c.id)}
                disabled={c.kind !== 'node' || c.op === 'deleted'}
                title={c.kind === 'node' && c.op !== 'deleted' ? 'Focus this node' : ''}
              >
                <span class="op {opClass(c.op)}">{c.op}</span>
                <span class="ck">{c.kind}</span>
                <span class="k">{c.record?.external_key ?? `#${c.id}`}</span>
                {#if c.labels?.length}<span class="cl">{c.labels.join(', ')}</span>{/if}
                <span class="cseq">@{c.seq}</span>
              </button>
            </li>
          {/each}
        </ol>
      {:else}
        <p class="live-empty">
          {feedPaused ? 'Paused.' : 'Watching for changes… commit something to see it here.'}
        </p>
      {/if}
    </div>
  {/if}

  <!-- The legend and the inspector share the right bar: the legend shows while
       nothing is selected; selecting a node/edge swaps it for the props layer. -->
  {#if !selected && (algoLegend.length || legend.length)}
    <div class="legend">
      <p class="legend-title">Legend</p>
      {#each (algoLegend.length ? algoLegend : legend) as e (e.label)}
        <span class="swatch" style="--c:{e.color}">{e.label || '(no label)'}</span>
      {/each}
    </div>
  {/if}

  {#if selected}
    <aside class="inspector">
      {#if selected.kind === 'node'}
        <h3>Node {selected.data.id}</h3>
        {#if selected.data.external_key}<p class="key">@{selected.data.external_key}</p>{/if}
        <p class="sub">{selected.data.labels?.join(', ') || '(no labels)'}</p>
      {:else}
        <h3>Edge {selected.data.id}</h3>
        <p class="sub">{selected.data.type} · {selected.data.src} → {selected.data.dst}</p>
      {/if}
      {#if !editing}
        <dl>
          {#each propEntries(selected.data.properties) as pe (pe.k)}
            <dt title={pe.desc ?? ''}>{pe.k}{#if pe.desc}<span class="badge" title={pe.desc}>ℹ</span>{/if}</dt>
            <dd>
              {#if isVector(pe.v)}
                <button class="vec-btn" onclick={() => (vectorView = { k: pe.k, values: pe.v })}>
                  show vector ({pe.v.length} dims)
                </button>
              {:else}
                {typeof pe.v === 'string' ? pe.v : JSON.stringify(pe.v)}
              {/if}
            </dd>
          {/each}
        </dl>
        <div class="inspector-actions">
          <button onclick={startEdit}>Edit</button>
          <button class="danger" onclick={askDelete}>Delete</button>
        </div>
      {:else}
        <div class="edit-field">
          {#if selected.kind === 'node'}
            <span>Labels</span>
            <input bind:value={draftLabels} placeholder="comma-separated" />
          {:else}
            <span>Type</span>
            <input bind:value={draftType} placeholder="edge type" />
          {/if}
        </div>
        <div class="edit-props">
          {#each draft as row, i (i)}
            <div class="edit-row">
              <input class="pk" value={row.key} readonly />
              <input class="pv" bind:value={row.value} />
              <button class="rm" title="remove property" onclick={() => removeDraftRow(i)}>×</button>
            </div>
          {/each}
          <div class="edit-row new">
            <input class="pk" placeholder="new key" bind:value={newKey} />
            <input class="pv" placeholder="value" bind:value={newValue} />
          </div>
        </div>
        {#if saveError}<p class="error">{saveError}</p>{/if}
        <p class="edit-note">Values parse as JSON (<code>42</code>, <code>true</code>, <code>"text"</code>); vectors &amp; _provenance are read-only.</p>
        <div class="inspector-actions">
          <button class="primary" onclick={saveEdit}>Save</button>
          <button onclick={cancelEdit}>Cancel</button>
        </div>
      {/if}
    </aside>
  {/if}

  {#if vectorView}
    <div class="modal-backdrop">
      <div class="modal">
        <header>
          <span>{vectorView.k} · {vectorView.values.length} dims</span>
          <button class="close" onclick={() => (vectorView = null)}>×</button>
        </header>
        <pre class="floats">{formatVector(vectorView.values)}</pre>
      </div>
    </div>
  {/if}
</div>

{#if confirmingDelete && selected}
  <div class="dlg-backdrop">
    <div class="dlg" role="dialog" aria-modal="true" aria-label="Delete {selected.kind}">
      <header>
        Delete {selected.kind}
        <button class="close" onclick={cancelDelete} aria-label="Close">×</button>
      </header>
      <div class="dlg-body">
        <p class="dlg-warn">
          This permanently deletes {selected.kind} <b>{deleteToken()}</b>{#if selected.kind === 'node'} and its incident edges{/if}. This cannot be undone.
        </p>
        <label class="dlg-confirm">
          Type <code>{deleteToken()}</code> to confirm
          <input
            bind:value={deleteInput}
            use:autofocus
            onkeydown={(e) => e.key === 'Enter' && confirmDelete()}
          />
        </label>
        {#if deleteError}<p class="dlg-error">{deleteError}</p>{/if}
        <div class="dlg-actions">
          <button class="ghost" onclick={cancelDelete}>Cancel</button>
          <button class="danger" onclick={confirmDelete} disabled={deleteInput.trim() !== deleteToken()}>
            Delete
          </button>
        </div>
      </div>
    </div>
  </div>
{/if}

<p class="hint">Click a node to expand and inspect it; click empty space to deselect. Drag from one node onto another to connect them. Hold the right mouse button to pan.</p>
