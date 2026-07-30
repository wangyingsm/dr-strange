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

    def plane_list(self) -> Any:
        """Every plane with its id, name, counts, and own properties.

        Access: read."""
        return self._call("plane.list")

    def plane_catalog(self, plane) -> Any:
        """One plane's soft schema (labels, property descriptions, edge types, counts).

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        return self._call("plane.catalog", _p)

    def node_get(self, plane, id=None, key=None) -> Any:
        """One node by id or external key; null if absent.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        if id is not None:
            _p["id"] = id
        if key is not None:
            _p["key"] = key
        return self._call("node.get", _p)

    def plane_neighbors(self, plane, id, direction=None, type=None) -> Any:
        """1-hop expansion as {node, edge} id pairs.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        _p["id"] = id
        if direction is not None:
            _p["direction"] = direction
        if type is not None:
            _p["type"] = type
        return self._call("plane.neighbors", _p)

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

    def plane_query(self, plane, plan) -> Any:
        """Run a serialized logical plan verbatim; returns scored rows.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        _p["plan"] = plan
        return self._call("plane.query", _p)

    def plane_cypher(self, plane, query, embed=None, params=None) -> Any:
        """Run a statement in the query language (openCypher subset). A read returns {nodes, edges, count}; a write (CREATE/MERGE/SET/REMOVE/DELETE) returns {write: true, ...change-counts}. Write-gated.

        Access: write."""
        _p: dict = {}
        _p["plane"] = plane
        _p["query"] = query
        if embed is not None:
            _p["embed"] = embed
        if params is not None:
            _p["params"] = params
        return self._call("plane.cypher", _p)

    def plane_find(self, plane, q, limit=None, semantic=None, provider=None, embed_model=None) -> Any:
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
        return self._call("plane.find", _p)

    def graph_seed(self, plane, label=None, limit=None) -> Any:
        """An initial canvas: up to `limit` nodes plus the edges induced among them.

        Access: read."""
        _p: dict = {}
        _p["plane"] = plane
        if label is not None:
            _p["label"] = label
        if limit is not None:
            _p["limit"] = limit
        return self._call("graph.seed", _p)

    def graph_expand(self, plane, id, direction=None, type=None, limit=None) -> Any:
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
        return self._call("graph.expand", _p)

    def digest_run(self, plane, text, chat=None, embed=None, model=None, embed_model=None, source=None, no_embed=None, link=None) -> Any:
        """Extract a node/edge proposal from text via the LLM (dry-run; spends provider credits).

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
