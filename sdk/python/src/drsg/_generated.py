# This file is GENERATED from crates/dr-strange-web/openrpc.json by codegen.py.
# Do not edit by hand — run `python codegen.py` to regenerate.
# ruff: noqa: A002  (params mirror wire names: id/type/set shadow builtins)
"""Typed dr-strange client, generated from the OpenRPC schema."""

from __future__ import annotations

from typing import Any

from ._client import _Client


class Drsg(_Client):
    """A dr-strange server client — one method per JSON-RPC method.

    ``Drsg(base_url=…, token=…)``; the token defaults to ``$DRSG_TOKEN``.
    """

    def rpc_discover(self) -> Any:
        """This OpenRPC service description.

        Access: read."""
        return self._call("rpc.discover")

    def db_stats(self) -> Any:
        """Plane/node/edge counts plus the on-disk file size when persistent.

        Access: read."""
        return self._call("db.stats")

    def db_catalog(self) -> Any:
        """The soft-schema catalog rolled up across every plane.

        Access: read."""
        return self._call("db.catalog")

    def plugin_list(self) -> Any:
        """Installed preprocessor plugins — the same records `drsg plugin list --json` prints, so an agent reads one shape from either surface (ROADMAP §11)."""
        return self._call("plugin.list")

    def plugin_catalog(self) -> Any:
        """The official plugin catalog, read from the extensions repository's catalog.json rather than compiled into this build — a plugin release needs no drsg release. Entries this build cannot run are returned tagged with why, not filtered out. Join against plugin.list to mark each installed/upgradable/absent. Cached for an hour; stale:true means the fetch failed and this is the last copy the store kept."""
        return self._call("plugin.catalog")

    def plugin_install(self, url) -> Any:
        """Download, validate, hash-pin and store a plugin from an http(s) URL. Write-gated; the URL passes the same resolved-address network policy as every other fetch. Server-local paths are deliberately not accepted over RPC."""
        _p: dict = {}
        _p["url"] = url
        return self._call("plugin.install", _p)

    def plugin_remove(self, name) -> Any:
        """Uninstall a plugin by name. Write-gated."""
        _p: dict = {}
        _p["name"] = name
        return self._call("plugin.remove", _p)

    def plane_list(self) -> Any:
        """Every plane with its id, name, counts, and own properties.

        Access: read."""
        return self._call("plane.list")

    def plane_vectorize(self, plane, embed=None, embed_model=None, metric=None) -> Any:
        """Embed every node in a plane (incremental by meaning — unchanged texts are skipped) and ensure a vector index on `embedding` per label. Same engine as `drsg vectorize`; the provider key comes from the server's environment."""
        _p: dict = {}
        _p["plane"] = plane
        if embed is not None:
            _p["embed"] = embed
        if embed_model is not None:
            _p["embed_model"] = embed_model
        if metric is not None:
            _p["metric"] = metric
        return self._call("plane.vectorize", _p)

    def plane_catalog(self, plane) -> Any:
        """One plane's soft schema (labels, property descriptions, edge types, counts).

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        return self._call("plane.catalog", _p)

    def node_get(self, plane, id=None, key=None, lean=None) -> Any:
        """One node by id or external key; null if absent.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        if id is not None:
            _p["id"] = id
        if key is not None:
            _p["key"] = key
        if lean is not None:
            _p["lean"] = lean
        return self._call("node.get", _p)

    def plane_neighbors(self, plane, id, direction=None, type=None, as_of=None, as_of_ms=None, hydrate=None, lean=None) -> Any:
        """1-hop expansion as {node, edge} id pairs.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        _p["id"] = id
        if direction is not None:
            _p["direction"] = direction
        if type is not None:
            _p["type"] = type
        if as_of is not None:
            _p["as_of"] = as_of
        if as_of_ms is not None:
            _p["as_of_ms"] = as_of_ms
        if hydrate is not None:
            _p["hydrate"] = hydrate
        if lean is not None:
            _p["lean"] = lean
        return self._call("plane.neighbors", _p)

    def plane_history(self) -> Any:
        """Time-travel window: oldest and latest commit sequences a read can be pinned to (native backend only).

        Access: read."""
        return self._call("plane.history")

    def plane_search(self, plane, property, query, label=None, k=None, metric=None) -> Any:
        """Vector top-k over a property; returns scored node records.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        _p["property"] = property
        _p["query"] = query
        if label is not None:
            _p["label"] = label
        if k is not None:
            _p["k"] = k
        if metric is not None:
            _p["metric"] = metric
        return self._call("plane.search", _p)

    def plane_query(self, plane, plan, as_of=None, as_of_ms=None) -> Any:
        """Run a serialized logical plan verbatim; returns scored rows.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        _p["plan"] = plan
        if as_of is not None:
            _p["as_of"] = as_of
        if as_of_ms is not None:
            _p["as_of_ms"] = as_of_ms
        return self._call("plane.query", _p)

    def plane_cypher(self, plane, query, embed=None, params=None, lean=None) -> Any:
        """Run a statement in the query language (openCypher subset). A read returns {nodes, edges, count}; a write (CREATE/MERGE/SET/REMOVE/DELETE) returns {write: true, ...change-counts}. Write-gated.

        Access: write."""
        _p: dict = {}
        _p["plane"] = plane
        _p["query"] = query
        if embed is not None:
            _p["embed"] = embed
        if params is not None:
            _p["params"] = params
        if lean is not None:
            _p["lean"] = lean
        return self._call("plane.cypher", _p)

    def plane_find(self, plane, q, limit=None, semantic=None, provider=None, embed_model=None, as_of=None, as_of_ms=None) -> Any:
        """Text (or semantic) search over the plane's nodes and edges.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        _p["q"] = q
        if limit is not None:
            _p["limit"] = limit
        if semantic is not None:
            _p["semantic"] = semantic
        if provider is not None:
            _p["provider"] = provider
        if embed_model is not None:
            _p["embed_model"] = embed_model
        if as_of is not None:
            _p["as_of"] = as_of
        if as_of_ms is not None:
            _p["as_of_ms"] = as_of_ms
        return self._call("plane.find", _p)

    def plane_algo(self, plane, algo, label=None, limit=None, damping=None, max_iters=None, tolerance=None, src=None, dst=None, dir=None, weight=None, max_levels=None, min_gain=None) -> Any:
        """Run a graph algorithm (pagerank | components | shortest_path | louvain) over the plane or one label subset, read-only over a single snapshot.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        _p["algo"] = algo
        if label is not None:
            _p["label"] = label
        if limit is not None:
            _p["limit"] = limit
        if damping is not None:
            _p["damping"] = damping
        if max_iters is not None:
            _p["max_iters"] = max_iters
        if tolerance is not None:
            _p["tolerance"] = tolerance
        if src is not None:
            _p["src"] = src
        if dst is not None:
            _p["dst"] = dst
        if dir is not None:
            _p["dir"] = dir
        if weight is not None:
            _p["weight"] = weight
        if max_levels is not None:
            _p["max_levels"] = max_levels
        if min_gain is not None:
            _p["min_gain"] = min_gain
        return self._call("plane.algo", _p)

    def plane_hybrid(self, plane, q, label=None, vector_prop=None, keyword_prop=None, metric=None, graph_hops=None, graph_decay=None, w_vector=None, w_keyword=None, w_graph=None, k=None, candidates=None, provider=None, embed_model=None) -> Any:
        """Hybrid retrieval: fuse vector similarity, BM25 keyword, and graph-proximity channels into one ranking. Enable a channel by naming its property (vector_prop/keyword_prop) or setting graph_hops; the vector channel embeds q server-side.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        _p["q"] = q
        if label is not None:
            _p["label"] = label
        if vector_prop is not None:
            _p["vector_prop"] = vector_prop
        if keyword_prop is not None:
            _p["keyword_prop"] = keyword_prop
        if metric is not None:
            _p["metric"] = metric
        if graph_hops is not None:
            _p["graph_hops"] = graph_hops
        if graph_decay is not None:
            _p["graph_decay"] = graph_decay
        if w_vector is not None:
            _p["w_vector"] = w_vector
        if w_keyword is not None:
            _p["w_keyword"] = w_keyword
        if w_graph is not None:
            _p["w_graph"] = w_graph
        if k is not None:
            _p["k"] = k
        if candidates is not None:
            _p["candidates"] = candidates
        if provider is not None:
            _p["provider"] = provider
        if embed_model is not None:
            _p["embed_model"] = embed_model
        return self._call("plane.hybrid", _p)

    def plane_ask(self, plane, question, dry_run=None, max_attempts=None, limit=None, provider=None, model=None, embed_provider=None, embed_model=None) -> Any:
        """Natural-language query: an LLM turns the question into a read-only LogicalPlan, runs it (unless dry_run), and returns the generated plan plus result node records. With embed_provider, the model can call find_edge/find_entity embedding tools to ground the plan. Keys from the server env.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        _p["question"] = question
        if dry_run is not None:
            _p["dry_run"] = dry_run
        if max_attempts is not None:
            _p["max_attempts"] = max_attempts
        if limit is not None:
            _p["limit"] = limit
        if provider is not None:
            _p["provider"] = provider
        if model is not None:
            _p["model"] = model
        if embed_provider is not None:
            _p["embed_provider"] = embed_provider
        if embed_model is not None:
            _p["embed_model"] = embed_model
        return self._call("plane.ask", _p)

    def plane_indexes(self, plane) -> Any:
        """The search indexes declared on a plane (vector + keyword), so a client can offer only the channels that actually exist.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        return self._call("plane.indexes", _p)

    def index_ensure(self, plane, label, property, kind=None, metric=None, language=None) -> Any:
        """Declare (and build) a search index on (label, property): a keyword (BM25) or vector (embedding) index. Idempotent.

        Access: admin."""
        _p: dict = {}
        _p["plane"] = plane
        _p["label"] = label
        _p["property"] = property
        if kind is not None:
            _p["kind"] = kind
        if metric is not None:
            _p["metric"] = metric
        if language is not None:
            _p["language"] = language
        return self._call("index.ensure", _p)

    def graph_seed(self, plane, label=None, limit=None, order=None, as_of=None, as_of_ms=None) -> Any:
        """An initial canvas of nodes plus induced edges. `order` seeds the highest-ranked nodes rather than the first the scan reaches — a legible skeleton instead of an arbitrary sample — and returns the scores alongside, so a caller can size or weight by importance without a second call.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        if label is not None:
            _p["label"] = label
        if limit is not None:
            _p["limit"] = limit
        if order is not None:
            _p["order"] = order
        if as_of is not None:
            _p["as_of"] = as_of
        if as_of_ms is not None:
            _p["as_of_ms"] = as_of_ms
        return self._call("graph.seed", _p)

    def graph_expand(self, plane, id, direction=None, type=None, limit=None, as_of=None, as_of_ms=None) -> Any:
        """Hub-safe 1-hop neighbourhood around a node: neighbour + connecting-edge records.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        _p["id"] = id
        if direction is not None:
            _p["direction"] = direction
        if type is not None:
            _p["type"] = type
        if limit is not None:
            _p["limit"] = limit
        if as_of is not None:
            _p["as_of"] = as_of
        if as_of_ms is not None:
            _p["as_of_ms"] = as_of_ms
        return self._call("graph.expand", _p)

    def digest_run(self, plane, text, chat=None, embed=None, model=None, embed_model=None, source=None, no_embed=None, link=None, concurrency=None, chunk_chars=None, mode=None) -> Any:
        """Extract a node/edge proposal from text via the LLM (dry-run; spends provider credits). `mode` sets how much clean-up follows the extraction: `coarse` reconciles the label and edge-type vocabularies, `fine` (the default) also merges entities that name the same thing, `super` also re-reads every entity against all the passages mentioning it — most accurate, and ~15x the input token usage.

        Access: write."""
        _p: dict = {}
        _p["plane"] = plane
        _p["text"] = text
        if chat is not None:
            _p["chat"] = chat
        if embed is not None:
            _p["embed"] = embed
        if model is not None:
            _p["model"] = model
        if embed_model is not None:
            _p["embed_model"] = embed_model
        if source is not None:
            _p["source"] = source
        if no_embed is not None:
            _p["no_embed"] = no_embed
        if link is not None:
            _p["link"] = link
        if concurrency is not None:
            _p["concurrency"] = concurrency
        if chunk_chars is not None:
            _p["chunk_chars"] = chunk_chars
        if mode is not None:
            _p["mode"] = mode
        return self._call("digest.run", _p)

    def digest_write(self, plane, nodes, edges=None) -> Any:
        """Write a previously-computed proposal into the plane via the bulk path (no LLM call).

        Access: write."""
        _p: dict = {}
        _p["plane"] = plane
        _p["nodes"] = nodes
        if edges is not None:
            _p["edges"] = edges
        return self._call("digest.write", _p)

    def node_create(self, plane, key=None, labels=None, properties=None) -> Any:
        """Add a node with an optional stable external key and labels.

        Access: write."""
        _p: dict = {}
        _p["plane"] = plane
        if key is not None:
            _p["key"] = key
        if labels is not None:
            _p["labels"] = labels
        if properties is not None:
            _p["properties"] = properties
        return self._call("node.create", _p)

    def node_update(self, plane, id=None, key=None, set=None, unset=None, labels=None) -> Any:
        """Patch a node: `set`/`unset` its properties, and `labels` (when present) replaces its label set.

        Access: write."""
        _p: dict = {}
        _p["plane"] = plane
        if id is not None:
            _p["id"] = id
        if key is not None:
            _p["key"] = key
        if set is not None:
            _p["set"] = set
        if unset is not None:
            _p["unset"] = unset
        if labels is not None:
            _p["labels"] = labels
        return self._call("node.update", _p)

    def node_delete(self, plane, id=None, key=None) -> Any:
        """Delete a node and cascade to its incident edges.

        Access: write."""
        _p: dict = {}
        _p["plane"] = plane
        if id is not None:
            _p["id"] = id
        if key is not None:
            _p["key"] = key
        return self._call("node.delete", _p)

    def edge_create(self, plane, src, dst, type, properties=None) -> Any:
        """Add a directed edge between two existing nodes (each named by id or key).

        Access: write."""
        _p: dict = {}
        _p["plane"] = plane
        _p["src"] = src
        _p["dst"] = dst
        _p["type"] = type
        if properties is not None:
            _p["properties"] = properties
        return self._call("edge.create", _p)

    def edge_update(self, plane, edge, set=None, unset=None, type=None) -> Any:
        """Patch an edge: `set`/`unset` its properties, and `type` (when present) changes its type.

        Access: write."""
        _p: dict = {}
        _p["plane"] = plane
        _p["edge"] = edge
        if set is not None:
            _p["set"] = set
        if unset is not None:
            _p["unset"] = unset
        if type is not None:
            _p["type"] = type
        return self._call("edge.update", _p)

    def edge_delete(self, plane, edge) -> Any:
        """Delete one edge.

        Access: write."""
        _p: dict = {}
        _p["plane"] = plane
        _p["edge"] = edge
        return self._call("edge.delete", _p)

    def plane_create(self, name, properties=None) -> Any:
        """Make a new, empty plane.

        Access: admin."""
        _p: dict = {}
        _p["name"] = name
        if properties is not None:
            _p["properties"] = properties
        return self._call("plane.create", _p)

    def plane_rename(self, plane, to) -> Any:
        """Rename an existing plane.

        Access: admin."""
        _p: dict = {}
        _p["plane"] = plane
        _p["to"] = to
        return self._call("plane.rename", _p)

    def plane_set_props(self, plane, properties) -> Any:
        """Replace a plane's own property map.

        Access: admin."""
        _p: dict = {}
        _p["plane"] = plane
        _p["properties"] = properties
        return self._call("plane.set_props", _p)

    def plane_delete(self, plane) -> Any:
        """Drop a plane and everything on it (the startup plane cannot be dropped).

        Access: admin."""
        _p: dict = {}
        _p["plane"] = plane
        return self._call("plane.delete", _p)
