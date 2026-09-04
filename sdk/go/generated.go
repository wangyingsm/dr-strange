// Code generated from crates/dr-strange-web/openrpc.json by internal/gen; DO NOT EDIT.

package drsg

import "context"

type DbStats struct {
	CommitSeq   int64  `json:"commit_seq"`
	EdgeTypes   int64  `json:"edge_types"`
	Edges       int64  `json:"edges"`
	FileSize    *int64 `json:"file_size,omitempty"`
	Indexes     int64  `json:"indexes"`
	Labels      int64  `json:"labels"`
	Nodes       int64  `json:"nodes"`
	Persistent  bool   `json:"persistent"`
	Planes      int64  `json:"planes"`
	PluginBytes int64  `json:"plugin_bytes"`
	RssBytes    *int64 `json:"rss_bytes,omitempty"`
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

type PluginListItem struct {
	Extensions []string `json:"extensions,omitempty"`
	File       *string  `json:"file,omitempty"`
	Name       *string  `json:"name,omitempty"`
	Sha256     *string  `json:"sha256,omitempty"`
	Source     *string  `json:"source,omitempty"`
	Version    *string  `json:"version,omitempty"`
}

type PluginCatalogResultPluginsItem struct {
	Claims  string `json:"claims"`
	Compat  string `json:"compat"`
	Name    string `json:"name"`
	Sha256  string `json:"sha256"`
	URL     string `json:"url"`
	Version string `json:"version"`
}

type PluginCatalogResult struct {
	Plugins []PluginCatalogResultPluginsItem `json:"plugins"`
	Schema  *int64                           `json:"schema,omitempty"`
	Source  map[string]any                   `json:"source,omitempty"`
	Stale   bool                             `json:"stale"`
}

type PluginInstallResult struct {
	Installed map[string]any `json:"installed,omitempty"`
	Replaced  *string        `json:"replaced,omitempty"`
}

type PluginInstallParams struct {
	URL string `json:"url"`
}

type PluginRemoveResult struct {
	Removed map[string]any `json:"removed,omitempty"`
}

type PluginRemoveParams struct {
	Name string `json:"name"`
}

type PlaneVectorizeResult struct {
	Current  *int64   `json:"current,omitempty"`
	Embedded *int64   `json:"embedded,omitempty"`
	Empty    *int64   `json:"empty,omitempty"`
	Labels   []string `json:"labels,omitempty"`
	Tokens   *int64   `json:"tokens,omitempty"`
	Unique   *int64   `json:"unique,omitempty"`
}

type PlaneVectorizeParams struct {
	Plane      string  `json:"plane"`
	Embed      *string `json:"embed,omitempty"`
	EmbedModel *string `json:"embed_model,omitempty"`
	Metric     *string `json:"metric,omitempty"`
}

type PlaneCatalogParams struct {
	Plane string `json:"plane"`
}

type NodeGetParams struct {
	Plane string  `json:"plane"`
	ID    *int64  `json:"id,omitempty"`
	Key   *string `json:"key,omitempty"`
	Lean  *bool   `json:"lean,omitempty"`
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
	AsOf      *int64  `json:"as_of,omitempty"`
	AsOfMs    *int64  `json:"as_of_ms,omitempty"`
	Hydrate   *bool   `json:"hydrate,omitempty"`
	Lean      *bool   `json:"lean,omitempty"`
}

type PlaneHistoryResult struct {
	Latest *int64 `json:"latest,omitempty"`
	Oldest *int64 `json:"oldest,omitempty"`
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
	Plane  string         `json:"plane"`
	Plan   map[string]any `json:"plan"`
	AsOf   *int64         `json:"as_of,omitempty"`
	AsOfMs *int64         `json:"as_of_ms,omitempty"`
}

type PlaneCypherParams struct {
	Plane  string         `json:"plane"`
	Query  string         `json:"query"`
	Embed  *string        `json:"embed,omitempty"`
	Params map[string]any `json:"params,omitempty"`
	Lean   *bool          `json:"lean,omitempty"`
}

type PlaneFindParams struct {
	Plane      string  `json:"plane"`
	Q          string  `json:"q"`
	Limit      *int64  `json:"limit,omitempty"`
	Semantic   *bool   `json:"semantic,omitempty"`
	Provider   *string `json:"provider,omitempty"`
	EmbedModel *string `json:"embed_model,omitempty"`
	AsOf       *int64  `json:"as_of,omitempty"`
	AsOfMs     *int64  `json:"as_of_ms,omitempty"`
}

type PlaneAlgoParams struct {
	Plane     string   `json:"plane"`
	Algo      string   `json:"algo"`
	Label     *string  `json:"label,omitempty"`
	Limit     *int64   `json:"limit,omitempty"`
	Damping   *float64 `json:"damping,omitempty"`
	MaxIters  *int64   `json:"max_iters,omitempty"`
	Tolerance *float64 `json:"tolerance,omitempty"`
	Src       *int64   `json:"src,omitempty"`
	Dst       *int64   `json:"dst,omitempty"`
	Dir       *string  `json:"dir,omitempty"`
	Weight    *string  `json:"weight,omitempty"`
	MaxLevels *int64   `json:"max_levels,omitempty"`
	MinGain   *float64 `json:"min_gain,omitempty"`
}

type PlaneHybridParams struct {
	Plane       string   `json:"plane"`
	Q           string   `json:"q"`
	Label       *string  `json:"label,omitempty"`
	VectorProp  *string  `json:"vector_prop,omitempty"`
	KeywordProp *string  `json:"keyword_prop,omitempty"`
	Metric      *string  `json:"metric,omitempty"`
	GraphHops   *int64   `json:"graph_hops,omitempty"`
	GraphDecay  *float64 `json:"graph_decay,omitempty"`
	WVector     *float64 `json:"w_vector,omitempty"`
	WKeyword    *float64 `json:"w_keyword,omitempty"`
	WGraph      *float64 `json:"w_graph,omitempty"`
	K           *int64   `json:"k,omitempty"`
	Candidates  *int64   `json:"candidates,omitempty"`
	Provider    *string  `json:"provider,omitempty"`
	EmbedModel  *string  `json:"embed_model,omitempty"`
}

type PlaneAskParams struct {
	Plane         string  `json:"plane"`
	Question      string  `json:"question"`
	DryRun        *bool   `json:"dry_run,omitempty"`
	MaxAttempts   *int64  `json:"max_attempts,omitempty"`
	Limit         *int64  `json:"limit,omitempty"`
	Provider      *string `json:"provider,omitempty"`
	Model         *string `json:"model,omitempty"`
	EmbedProvider *string `json:"embed_provider,omitempty"`
	EmbedModel    *string `json:"embed_model,omitempty"`
}

type PlaneIndexesParams struct {
	Plane string `json:"plane"`
}

type IndexEnsureParams struct {
	Plane    string  `json:"plane"`
	Label    string  `json:"label"`
	Property string  `json:"property"`
	Kind     *string `json:"kind,omitempty"`
	Metric   *string `json:"metric,omitempty"`
	Language *string `json:"language,omitempty"`
}

type GraphSeedParams struct {
	Plane  string  `json:"plane"`
	Label  *string `json:"label,omitempty"`
	Limit  *int64  `json:"limit,omitempty"`
	Order  *string `json:"order,omitempty"`
	AsOf   *int64  `json:"as_of,omitempty"`
	AsOfMs *int64  `json:"as_of_ms,omitempty"`
}

type GraphExpandParams struct {
	Plane     string  `json:"plane"`
	ID        int64   `json:"id"`
	Direction *string `json:"direction,omitempty"`
	Type      *string `json:"type,omitempty"`
	Limit     *int64  `json:"limit,omitempty"`
	AsOf      *int64  `json:"as_of,omitempty"`
	AsOfMs    *int64  `json:"as_of_ms,omitempty"`
}

type DigestRunParams struct {
	Plane       string  `json:"plane"`
	Text        string  `json:"text"`
	Chat        *string `json:"chat,omitempty"`
	Embed       *string `json:"embed,omitempty"`
	Model       *string `json:"model,omitempty"`
	EmbedModel  *string `json:"embed_model,omitempty"`
	Source      *string `json:"source,omitempty"`
	NoEmbed     *bool   `json:"no_embed,omitempty"`
	Link        *bool   `json:"link,omitempty"`
	Concurrency *int64  `json:"concurrency,omitempty"`
	ChunkChars  *int64  `json:"chunk_chars,omitempty"`
	Mode        *string `json:"mode,omitempty"`
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

// PluginList Installed preprocessor plugins — the same records `drsg plugin list --json` prints, so an agent reads one shape from either surface (ROADMAP §11).
func (c *Client) PluginList(ctx context.Context) ([]PluginListItem, error) {
	var out []PluginListItem
	err := c.call(ctx, "plugin.list", nil, &out)
	return out, err
}

// PluginCatalog The official plugin catalog, read from the extensions repository's catalog.json rather than compiled into this build — a plugin release needs no drsg release. Entries this build cannot run are returned tagged with why, not filtered out. Join against plugin.list to mark each installed/upgradable/absent. Cached for an hour; stale:true means the fetch failed and this is the last copy the store kept.
func (c *Client) PluginCatalog(ctx context.Context) (*PluginCatalogResult, error) {
	var out *PluginCatalogResult
	err := c.call(ctx, "plugin.catalog", nil, &out)
	return out, err
}

// PluginInstall Download, validate, hash-pin and store a plugin from an http(s) URL. Write-gated; the URL passes the same resolved-address network policy as every other fetch. Server-local paths are deliberately not accepted over RPC.
func (c *Client) PluginInstall(ctx context.Context, p PluginInstallParams) (*PluginInstallResult, error) {
	var out *PluginInstallResult
	err := c.call(ctx, "plugin.install", p, &out)
	return out, err
}

// PluginRemove Uninstall a plugin by name. Write-gated.
func (c *Client) PluginRemove(ctx context.Context, p PluginRemoveParams) (*PluginRemoveResult, error) {
	var out *PluginRemoveResult
	err := c.call(ctx, "plugin.remove", p, &out)
	return out, err
}

// PlaneList Every plane with its id, name, counts, and own properties. (access: read)
func (c *Client) PlaneList(ctx context.Context) ([]PlaneCard, error) {
	var out []PlaneCard
	err := c.call(ctx, "plane.list", nil, &out)
	return out, err
}

// PlaneVectorize Embed every node in a plane (incremental by meaning — unchanged texts are skipped) and ensure a vector index on `embedding` per label. Same engine as `drsg vectorize`; the provider key comes from the server's environment.
func (c *Client) PlaneVectorize(ctx context.Context, p PlaneVectorizeParams) (*PlaneVectorizeResult, error) {
	var out *PlaneVectorizeResult
	err := c.call(ctx, "plane.vectorize", p, &out)
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

// PlaneHistory Time-travel window: oldest and latest commit sequences a read can be pinned to (native backend only). (access: read)
func (c *Client) PlaneHistory(ctx context.Context) (*PlaneHistoryResult, error) {
	var out *PlaneHistoryResult
	err := c.call(ctx, "plane.history", nil, &out)
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

// PlaneCypher Run a statement in the query language (openCypher subset). A read returns {nodes, edges, count}; a write (CREATE/MERGE/SET/REMOVE/DELETE) returns {write: true, ...change-counts}. Write-gated. (access: write)
func (c *Client) PlaneCypher(ctx context.Context, p PlaneCypherParams) (map[string]any, error) {
	var out map[string]any
	err := c.call(ctx, "plane.cypher", p, &out)
	return out, err
}

// PlaneFind Text (or semantic) search over the plane's nodes and edges. (access: read)
func (c *Client) PlaneFind(ctx context.Context, p PlaneFindParams) (*FindResult, error) {
	var out *FindResult
	err := c.call(ctx, "plane.find", p, &out)
	return out, err
}

// PlaneAlgo Run a graph algorithm (pagerank | components | shortest_path | louvain) over the plane or one label subset, read-only over a single snapshot. (access: read)
func (c *Client) PlaneAlgo(ctx context.Context, p PlaneAlgoParams) (map[string]any, error) {
	var out map[string]any
	err := c.call(ctx, "plane.algo", p, &out)
	return out, err
}

// PlaneHybrid Hybrid retrieval: fuse vector similarity, BM25 keyword, and graph-proximity channels into one ranking. Enable a channel by naming its property (vector_prop/keyword_prop) or setting graph_hops; the vector channel embeds q server-side. (access: read)
func (c *Client) PlaneHybrid(ctx context.Context, p PlaneHybridParams) (map[string]any, error) {
	var out map[string]any
	err := c.call(ctx, "plane.hybrid", p, &out)
	return out, err
}

// PlaneAsk Natural-language query: an LLM turns the question into a read-only LogicalPlan, runs it (unless dry_run), and returns the generated plan plus result node records. With embed_provider, the model can call find_edge/find_entity embedding tools to ground the plan. Keys from the server env. (access: read)
func (c *Client) PlaneAsk(ctx context.Context, p PlaneAskParams) (map[string]any, error) {
	var out map[string]any
	err := c.call(ctx, "plane.ask", p, &out)
	return out, err
}

// PlaneIndexes The search indexes declared on a plane (vector + keyword), so a client can offer only the channels that actually exist. (access: read)
func (c *Client) PlaneIndexes(ctx context.Context, p PlaneIndexesParams) (map[string]any, error) {
	var out map[string]any
	err := c.call(ctx, "plane.indexes", p, &out)
	return out, err
}

// IndexEnsure Declare (and build) a search index on (label, property): a keyword (BM25) or vector (embedding) index. Idempotent. (access: admin)
func (c *Client) IndexEnsure(ctx context.Context, p IndexEnsureParams) (map[string]any, error) {
	var out map[string]any
	err := c.call(ctx, "index.ensure", p, &out)
	return out, err
}

// GraphSeed An initial canvas of nodes plus induced edges. `order` seeds the highest-ranked nodes rather than the first the scan reaches — a legible skeleton instead of an arbitrary sample — and returns the scores alongside, so a caller can size or weight by importance without a second call. (access: read)
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

// DigestRun Extract a node/edge proposal from text via the LLM (dry-run; spends provider credits). `mode` sets how much clean-up follows the extraction: `coarse` reconciles the label and edge-type vocabularies, `fine` (the default) also merges entities that name the same thing, `super` also re-reads every entity against all the passages mentioning it — most accurate, and ~15x the input token usage. (access: write)
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
