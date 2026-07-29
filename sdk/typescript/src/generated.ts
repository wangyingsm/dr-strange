// This file is GENERATED from crates/dr-strange-web/openrpc.json by codegen.mjs.
// Do not edit by hand — run `bun run codegen` to regenerate.
import { Client } from "./client";

/** Property map in the core JSON dialect: scalars map directly; {"$vector":[…]} is an embedding; {"$desc":…,"$value":…} is a described value. */
export type Properties = Record<string, unknown>;

/** A node reference: a numeric id or an external key. */
export type NodeRef = number | string;

export interface NodeRecord {
  id: number;
  external_key?: string | null;
  labels: Array<string>;
  properties: Properties;
  score?: number;
  match?: string;
}

export interface EdgeRecord {
  id: number;
  src: number;
  dst: number;
  type: string;
  properties: Properties;
  match?: string;
}

export interface Subgraph {
  nodes: Array<NodeRecord>;
  edges: Array<EdgeRecord>;
  total: number;
  truncated: boolean;
}

export interface FindResult {
  nodes: Array<NodeRecord>;
  edges: Array<EdgeRecord>;
  mode: "text" | "semantic";
  note?: string | null;
  scanned?: number;
  total?: number;
  truncated?: boolean;
}

export interface Deleted {
  deleted: boolean;
  id?: number;
}

export interface PlaneRef {
  id: number;
  name: string;
}

export interface PlaneCard {
  id: number;
  name: string;
  nodes: number;
  edges: number;
  properties?: Properties;
}

export interface DbStats {
  planes: number;
  nodes: number;
  edges: number;
  persistent: boolean;
  file_size?: number | null;
}

/** A dr-strange server client — one method per JSON-RPC method. */
export class Drsg extends Client {
  /** This OpenRPC service description. (access: read) */
  rpcDiscover(): Promise<Record<string, unknown>> {
    return this._call("rpc.discover") as Promise<Record<string, unknown>>;
  }

  /** Plane/node/edge counts plus the on-disk file size when persistent. (access: read) */
  dbStats(): Promise<DbStats> {
    return this._call("db.stats") as Promise<DbStats>;
  }

  /** The soft-schema catalog rolled up across every plane. (access: read) */
  dbCatalog(): Promise<Record<string, unknown>> {
    return this._call("db.catalog") as Promise<Record<string, unknown>>;
  }

  /** Every plane with its id, name, counts, and own properties. (access: read) */
  planeList(): Promise<Array<PlaneCard>> {
    return this._call("plane.list") as Promise<Array<PlaneCard>>;
  }

  /** One plane's soft schema (labels, property descriptions, edge types, counts). (access: read) */
  planeCatalog(params: { plane: string }): Promise<Record<string, unknown>> {
    return this._call("plane.catalog", params) as Promise<Record<string, unknown>>;
  }

  /** One node by id or external key; null if absent. (access: read) */
  nodeGet(params: { plane: string; id?: number; key?: string }): Promise<NodeRecord | null> {
    return this._call("node.get", params) as Promise<NodeRecord | null>;
  }

  /** 1-hop expansion as {node, edge} id pairs. (access: read) */
  planeNeighbors(params: { plane: string; id: number; direction?: "out" | "in" | "both"; type?: string }): Promise<Array<{ node?: number; edge?: number }>> {
    return this._call("plane.neighbors", params) as Promise<Array<{ node?: number; edge?: number }>>;
  }

  /** Vector top-k over a property; returns scored node records. (access: read) */
  planeSearch(params: { plane: string; property: string; query: Array<number>; label?: string; k?: number; metric?: "cosine" | "dot" | "l2" }): Promise<Array<NodeRecord>> {
    return this._call("plane.search", params) as Promise<Array<NodeRecord>>;
  }

  /** Run a serialized logical plan verbatim; returns scored rows. (access: read) */
  planeQuery(params: { plane: string; plan: Record<string, unknown> }): Promise<Array<NodeRecord>> {
    return this._call("plane.query", params) as Promise<Array<NodeRecord>>;
  }

  /** Text (or semantic) search over the plane's nodes and edges. (access: read) */
  planeFind(params: { plane: string; q: string; limit?: number; semantic?: boolean; provider?: string; embed_model?: string }): Promise<FindResult> {
    return this._call("plane.find", params) as Promise<FindResult>;
  }

  /** An initial canvas: up to `limit` nodes plus the edges induced among them. (access: read) */
  graphSeed(params: { plane: string; label?: string; limit?: number }): Promise<Subgraph> {
    return this._call("graph.seed", params) as Promise<Subgraph>;
  }

  /** Hub-safe 1-hop neighbourhood around a node: neighbour + connecting-edge records. (access: read) */
  graphExpand(params: { plane: string; id: number; direction?: "out" | "in" | "both"; type?: string; limit?: number }): Promise<Subgraph> {
    return this._call("graph.expand", params) as Promise<Subgraph>;
  }

  /** Extract a node/edge proposal from text via the LLM (dry-run; spends provider credits). (access: write) */
  digestRun(params: { plane: string; text: string; chat?: string; embed?: string; model?: string; embed_model?: string; source?: string; no_embed?: boolean; link?: boolean }): Promise<Record<string, unknown>> {
    return this._call("digest.run", params) as Promise<Record<string, unknown>>;
  }

  /** Write a previously-computed proposal into the plane via the bulk path (no LLM call). (access: write) */
  digestWrite(params: { plane: string; nodes: Array<Record<string, unknown>>; edges?: Array<Record<string, unknown>> }): Promise<{ nodes_written?: number; edges_written?: number }> {
    return this._call("digest.write", params) as Promise<{ nodes_written?: number; edges_written?: number }>;
  }

  /** Add a node with an optional stable external key and labels. (access: write) */
  nodeCreate(params: { plane: string; key?: string; labels?: Array<string>; properties?: Properties }): Promise<NodeRecord> {
    return this._call("node.create", params) as Promise<NodeRecord>;
  }

  /** Patch a node: `set`/`unset` its properties, and `labels` (when present) replaces its label set. (access: write) */
  nodeUpdate(params: { plane: string; id?: number; key?: string; set?: Properties; unset?: Array<string>; labels?: Array<string> }): Promise<NodeRecord> {
    return this._call("node.update", params) as Promise<NodeRecord>;
  }

  /** Delete a node and cascade to its incident edges. (access: write) */
  nodeDelete(params: { plane: string; id?: number; key?: string }): Promise<Deleted> {
    return this._call("node.delete", params) as Promise<Deleted>;
  }

  /** Add a directed edge between two existing nodes (each named by id or key). (access: write) */
  edgeCreate(params: { plane: string; src: NodeRef; dst: NodeRef; type: string; properties?: Properties }): Promise<EdgeRecord> {
    return this._call("edge.create", params) as Promise<EdgeRecord>;
  }

  /** Patch an edge: `set`/`unset` its properties, and `type` (when present) changes its type. (access: write) */
  edgeUpdate(params: { plane: string; edge: number; set?: Properties; unset?: Array<string>; type?: string }): Promise<EdgeRecord> {
    return this._call("edge.update", params) as Promise<EdgeRecord>;
  }

  /** Delete one edge. (access: write) */
  edgeDelete(params: { plane: string; edge: number }): Promise<Deleted> {
    return this._call("edge.delete", params) as Promise<Deleted>;
  }

  /** Make a new, empty plane. (access: admin) */
  planeCreate(params: { name: string; properties?: Properties }): Promise<PlaneRef> {
    return this._call("plane.create", params) as Promise<PlaneRef>;
  }

  /** Rename an existing plane. (access: admin) */
  planeRename(params: { plane: string; to: string }): Promise<PlaneRef> {
    return this._call("plane.rename", params) as Promise<PlaneRef>;
  }

  /** Replace a plane's own property map. (access: admin) */
  planeSetProps(params: { plane: string; properties: Properties }): Promise<Record<string, unknown>> {
    return this._call("plane.set_props", params) as Promise<Record<string, unknown>>;
  }

  /** Drop a plane and everything on it (the startup plane cannot be dropped). (access: admin) */
  planeDelete(params: { plane: string }): Promise<Deleted> {
    return this._call("plane.delete", params) as Promise<Deleted>;
  }
}
