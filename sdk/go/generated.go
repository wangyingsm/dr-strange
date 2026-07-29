// Code generated from crates/dr-strange-web/openrpc.json by internal/gen; DO NOT EDIT.

package drsg

import "context"

type DbStats struct {
	Edges      int64  `json:"edges"`
	FileSize   *int64 `json:"file_size,omitempty"`
	Nodes      int64  `json:"nodes"`
	Persistent bool   `json:"persistent"`
	Planes     int64  `json:"planes"`
}

type Deleted struct {
	Deleted bool   `json:"deleted"`
	ID      *int64 `json:"id,omitempty"`
}

type EdgeRecord struct {
	Dst        int64      `json:"dst"`
	ID         int64      `json:"id"`
	Match      *string    `json:"match,omitempty"`
	Properties Properties `json:"properties"`
	Src        int64      `json:"src"`
	Type       string     `json:"type"`
}

type FindResult struct {
	Edges     []EdgeRecord `json:"edges"`
	Mode      string       `json:"mode"`
	Nodes     []NodeRecord `json:"nodes"`
	Note      *string      `json:"note,omitempty"`
	Scanned   *int64       `json:"scanned,omitempty"`
	Total     *int64       `json:"total,omitempty"`
	Truncated *bool        `json:"truncated,omitempty"`
}

type NodeRecord struct {
	ExternalKey *string    `json:"external_key,omitempty"`
	ID          int64      `json:"id"`
	Labels      []string   `json:"labels"`
	Match       *string    `json:"match,omitempty"`
	Properties  Properties `json:"properties"`
	Score       *float64   `json:"score,omitempty"`
}

// NodeRef — A node reference: a numeric id or an external key.
type NodeRef = any

type PlaneCard struct {
	Edges      int64      `json:"edges"`
	ID         int64      `json:"id"`
	Name       string     `json:"name"`
	Nodes      int64      `json:"nodes"`
	Properties Properties `json:"properties,omitempty"`
}

type PlaneRef struct {
	ID   int64  `json:"id"`
	Name string `json:"name"`
}

// Properties — Property map in the core JSON dialect: scalars map directly; {"$vector":[…]} is an embedding; {"$desc":…,"$value":…} is a described value.
type Properties = map[string]any

type Subgraph struct {
	Edges     []EdgeRecord `json:"edges"`
	Nodes     []NodeRecord `json:"nodes"`
	Total     int64        `json:"total"`
	Truncated bool         `json:"truncated"`
}

type PlaneCatalogParams struct {
	Plane string `json:"plane"`
}

type NodeGetParams struct {
	Plane string  `json:"plane"`
	ID    *int64  `json:"id,omitempty"`
	Key   *string `json:"key,omitempty"`
}

type PlaneNeighborsItem struct {
	Edge *int64 `json:"edge,omitempty"`
	Node *int64 `json:"node,omitempty"`
}

type PlaneNeighborsParams struct {
	Plane     string  `json:"plane"`
	ID        int64   `json:"id"`
	Direction *string `json:"direction,omitempty"`
	Type      *string `json:"type,omitempty"`
}

type PlaneSearchParams struct {
	Plane    string    `json:"plane"`
	Property string    `json:"property"`
	Query    []float64 `json:"query"`
	Label    *string   `json:"label,omitempty"`
	K        *int64    `json:"k,omitempty"`
	Metric   *string   `json:"metric,omitempty"`
}

type PlaneQueryParams struct {
	Plane string         `json:"plane"`
	Plan  map[string]any `json:"plan"`
}

type PlaneFindParams struct {
	Plane      string  `json:"plane"`
	Q          string  `json:"q"`
	Limit      *int64  `json:"limit,omitempty"`
	Semantic   *bool   `json:"semantic,omitempty"`
	Provider   *string `json:"provider,omitempty"`
	EmbedModel *string `json:"embed_model,omitempty"`
}

type GraphSeedParams struct {
	Plane string  `json:"plane"`
	Label *string `json:"label,omitempty"`
	Limit *int64  `json:"limit,omitempty"`
}

type GraphExpandParams struct {
	Plane     string  `json:"plane"`
	ID        int64   `json:"id"`
	Direction *string `json:"direction,omitempty"`
	Type      *string `json:"type,omitempty"`
	Limit     *int64  `json:"limit,omitempty"`
}

type DigestRunParams struct {
	Plane      string  `json:"plane"`
	Text       string  `json:"text"`
	Chat       *string `json:"chat,omitempty"`
	Embed      *string `json:"embed,omitempty"`
	Model      *string `json:"model,omitempty"`
	EmbedModel *string `json:"embed_model,omitempty"`
	Source     *string `json:"source,omitempty"`
	NoEmbed    *bool   `json:"no_embed,omitempty"`
	Link       *bool   `json:"link,omitempty"`
}

type DigestWriteResult struct {
	EdgesWritten *int64 `json:"edges_written,omitempty"`
	NodesWritten *int64 `json:"nodes_written,omitempty"`
}

type DigestWriteParams struct {
	Plane string           `json:"plane"`
	Nodes []map[string]any `json:"nodes"`
	Edges []map[string]any `json:"edges,omitempty"`
}

type NodeCreateParams struct {
	Plane      string     `json:"plane"`
	Key        *string    `json:"key,omitempty"`
	Labels     []string   `json:"labels,omitempty"`
	Properties Properties `json:"properties,omitempty"`
}

type NodeUpdateParams struct {
	Plane  string     `json:"plane"`
	ID     *int64     `json:"id,omitempty"`
	Key    *string    `json:"key,omitempty"`
	Set    Properties `json:"set,omitempty"`
	Unset  []string   `json:"unset,omitempty"`
	Labels []string   `json:"labels,omitempty"`
}

type NodeDeleteParams struct {
	Plane string  `json:"plane"`
	ID    *int64  `json:"id,omitempty"`
	Key   *string `json:"key,omitempty"`
}

type EdgeCreateParams struct {
	Plane      string     `json:"plane"`
	Src        NodeRef    `json:"src"`
	Dst        NodeRef    `json:"dst"`
	Type       string     `json:"type"`
	Properties Properties `json:"properties,omitempty"`
}

type EdgeUpdateParams struct {
	Plane string     `json:"plane"`
	Edge  int64      `json:"edge"`
	Set   Properties `json:"set,omitempty"`
	Unset []string   `json:"unset,omitempty"`
	Type  *string    `json:"type,omitempty"`
}

type EdgeDeleteParams struct {
	Plane string `json:"plane"`
	Edge  int64  `json:"edge"`
}

type PlaneCreateParams struct {
	Name       string     `json:"name"`
	Properties Properties `json:"properties,omitempty"`
}

type PlaneRenameParams struct {
	Plane string `json:"plane"`
	To    string `json:"to"`
}

type PlaneSetPropsParams struct {
	Plane      string     `json:"plane"`
	Properties Properties `json:"properties"`
}

type PlaneDeleteParams struct {
	Plane string `json:"plane"`
}

// RpcDiscover This OpenRPC service description. (access: read)
func (c *Client) RpcDiscover(ctx context.Context) (map[string]any, error) {
	var out map[string]any
	err := c.call(ctx, "rpc.discover", nil, &out)
	return out, err
}

// DbStats Plane/node/edge counts plus the on-disk file size when persistent. (access: read)
func (c *Client) DbStats(ctx context.Context) (*DbStats, error) {
	var out *DbStats
	err := c.call(ctx, "db.stats", nil, &out)
	return out, err
}

// DbCatalog The soft-schema catalog rolled up across every plane. (access: read)
func (c *Client) DbCatalog(ctx context.Context) (map[string]any, error) {
	var out map[string]any
	err := c.call(ctx, "db.catalog", nil, &out)
	return out, err
}

// PlaneList Every plane with its id, name, counts, and own properties. (access: read)
func (c *Client) PlaneList(ctx context.Context) ([]PlaneCard, error) {
	var out []PlaneCard
	err := c.call(ctx, "plane.list", nil, &out)
	return out, err
}

// PlaneCatalog One plane's soft schema (labels, property descriptions, edge types, counts). (access: read)
func (c *Client) PlaneCatalog(ctx context.Context, p PlaneCatalogParams) (map[string]any, error) {
	var out map[string]any
	err := c.call(ctx, "plane.catalog", p, &out)
	return out, err
}

// NodeGet One node by id or external key; null if absent. (access: read)
func (c *Client) NodeGet(ctx context.Context, p NodeGetParams) (*NodeRecord, error) {
	var out *NodeRecord
	err := c.call(ctx, "node.get", p, &out)
	return out, err
}

// PlaneNeighbors 1-hop expansion as {node, edge} id pairs. (access: read)
func (c *Client) PlaneNeighbors(ctx context.Context, p PlaneNeighborsParams) ([]PlaneNeighborsItem, error) {
	var out []PlaneNeighborsItem
	err := c.call(ctx, "plane.neighbors", p, &out)
	return out, err
}

// PlaneSearch Vector top-k over a property; returns scored node records. (access: read)
func (c *Client) PlaneSearch(ctx context.Context, p PlaneSearchParams) ([]NodeRecord, error) {
	var out []NodeRecord
	err := c.call(ctx, "plane.search", p, &out)
	return out, err
}

// PlaneQuery Run a serialized logical plan verbatim; returns scored rows. (access: read)
func (c *Client) PlaneQuery(ctx context.Context, p PlaneQueryParams) ([]NodeRecord, error) {
	var out []NodeRecord
	err := c.call(ctx, "plane.query", p, &out)
	return out, err
}

// PlaneFind Text (or semantic) search over the plane's nodes and edges. (access: read)
func (c *Client) PlaneFind(ctx context.Context, p PlaneFindParams) (*FindResult, error) {
	var out *FindResult
	err := c.call(ctx, "plane.find", p, &out)
	return out, err
}

// GraphSeed An initial canvas: up to `limit` nodes plus the edges induced among them. (access: read)
func (c *Client) GraphSeed(ctx context.Context, p GraphSeedParams) (*Subgraph, error) {
	var out *Subgraph
	err := c.call(ctx, "graph.seed", p, &out)
	return out, err
}

// GraphExpand Hub-safe 1-hop neighbourhood around a node: neighbour + connecting-edge records. (access: read)
func (c *Client) GraphExpand(ctx context.Context, p GraphExpandParams) (*Subgraph, error) {
	var out *Subgraph
	err := c.call(ctx, "graph.expand", p, &out)
	return out, err
}

// DigestRun Extract a node/edge proposal from text via the LLM (dry-run; spends provider credits). (access: write)
func (c *Client) DigestRun(ctx context.Context, p DigestRunParams) (map[string]any, error) {
	var out map[string]any
	err := c.call(ctx, "digest.run", p, &out)
	return out, err
}

// DigestWrite Write a previously-computed proposal into the plane via the bulk path (no LLM call). (access: write)
func (c *Client) DigestWrite(ctx context.Context, p DigestWriteParams) (*DigestWriteResult, error) {
	var out *DigestWriteResult
	err := c.call(ctx, "digest.write", p, &out)
	return out, err
}

// NodeCreate Add a node with an optional stable external key and labels. (access: write)
func (c *Client) NodeCreate(ctx context.Context, p NodeCreateParams) (*NodeRecord, error) {
	var out *NodeRecord
	err := c.call(ctx, "node.create", p, &out)
	return out, err
}

// NodeUpdate Patch a node: `set`/`unset` its properties, and `labels` (when present) replaces its label set. (access: write)
func (c *Client) NodeUpdate(ctx context.Context, p NodeUpdateParams) (*NodeRecord, error) {
	var out *NodeRecord
	err := c.call(ctx, "node.update", p, &out)
	return out, err
}

// NodeDelete Delete a node and cascade to its incident edges. (access: write)
func (c *Client) NodeDelete(ctx context.Context, p NodeDeleteParams) (*Deleted, error) {
	var out *Deleted
	err := c.call(ctx, "node.delete", p, &out)
	return out, err
}

// EdgeCreate Add a directed edge between two existing nodes (each named by id or key). (access: write)
func (c *Client) EdgeCreate(ctx context.Context, p EdgeCreateParams) (*EdgeRecord, error) {
	var out *EdgeRecord
	err := c.call(ctx, "edge.create", p, &out)
	return out, err
}

// EdgeUpdate Patch an edge: `set`/`unset` its properties, and `type` (when present) changes its type. (access: write)
func (c *Client) EdgeUpdate(ctx context.Context, p EdgeUpdateParams) (*EdgeRecord, error) {
	var out *EdgeRecord
	err := c.call(ctx, "edge.update", p, &out)
	return out, err
}

// EdgeDelete Delete one edge. (access: write)
func (c *Client) EdgeDelete(ctx context.Context, p EdgeDeleteParams) (*Deleted, error) {
	var out *Deleted
	err := c.call(ctx, "edge.delete", p, &out)
	return out, err
}

// PlaneCreate Make a new, empty plane. (access: admin)
func (c *Client) PlaneCreate(ctx context.Context, p PlaneCreateParams) (*PlaneRef, error) {
	var out *PlaneRef
	err := c.call(ctx, "plane.create", p, &out)
	return out, err
}

// PlaneRename Rename an existing plane. (access: admin)
func (c *Client) PlaneRename(ctx context.Context, p PlaneRenameParams) (*PlaneRef, error) {
	var out *PlaneRef
	err := c.call(ctx, "plane.rename", p, &out)
	return out, err
}

// PlaneSetProps Replace a plane's own property map. (access: admin)
func (c *Client) PlaneSetProps(ctx context.Context, p PlaneSetPropsParams) (map[string]any, error) {
	var out map[string]any
	err := c.call(ctx, "plane.set_props", p, &out)
	return out, err
}

// PlaneDelete Drop a plane and everything on it (the startup plane cannot be dropped). (access: admin)
func (c *Client) PlaneDelete(ctx context.Context, p PlaneDeleteParams) (*Deleted, error) {
	var out *Deleted
	err := c.call(ctx, "plane.delete", p, &out)
	return out, err
}
