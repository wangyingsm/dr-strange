// Code generated from crates/dr-strange-web/openrpc.json by codegen.c; DO NOT EDIT.
#ifndef DRSG_GENERATED_H
#define DRSG_GENERATED_H

#include "drsg.h"

/* This OpenRPC service description. (access: read) */
struct json_object *drsg_rpc_discover(drsg_client *c, drsg_error *err);

/* Plane/node/edge counts plus the on-disk file size when persistent. (access: read) */
struct json_object *drsg_db_stats(drsg_client *c, drsg_error *err);

/* The soft-schema catalog rolled up across every plane. (access: read) */
struct json_object *drsg_db_catalog(drsg_client *c, drsg_error *err);

/* Every plane with its id, name, counts, and own properties. (access: read) */
struct json_object *drsg_plane_list(drsg_client *c, drsg_error *err);

/* One plane's soft schema (labels, property descriptions, edge types, counts). (access: read) */
struct json_object *drsg_plane_catalog(drsg_client *c, const char *plane, drsg_error *err);

typedef struct {
    const int64_t *id;
    const char *key;
} drsg_node_get_opts;

/* One node by id or external key; null if absent. (access: read) */
struct json_object *drsg_node_get(drsg_client *c, const char *plane, const drsg_node_get_opts *opts, drsg_error *err);

typedef struct {
    const char *direction;
    const char *type;
    const int64_t *as_of;
    const int64_t *as_of_ms;
} drsg_plane_neighbors_opts;

/* 1-hop expansion as {node, edge} id pairs. (access: read) */
struct json_object *drsg_plane_neighbors(drsg_client *c, const char *plane, int64_t id, const drsg_plane_neighbors_opts *opts, drsg_error *err);

/* Time-travel window: oldest and latest commit sequences a read can be pinned to (native backend only). (access: read) */
struct json_object *drsg_plane_history(drsg_client *c, drsg_error *err);

typedef struct {
    const char *label;
    const int64_t *k;
    const char *metric;
} drsg_plane_search_opts;

/* Vector top-k over a property; returns scored node records. (access: read) */
struct json_object *drsg_plane_search(drsg_client *c, const char *plane, const char *property, struct json_object *query, const drsg_plane_search_opts *opts, drsg_error *err);

typedef struct {
    const int64_t *as_of;
    const int64_t *as_of_ms;
} drsg_plane_query_opts;

/* Run a serialized logical plan verbatim; returns scored rows. (access: read) */
struct json_object *drsg_plane_query(drsg_client *c, const char *plane, struct json_object *plan, const drsg_plane_query_opts *opts, drsg_error *err);

typedef struct {
    const char *embed;
    struct json_object *params;
} drsg_plane_cypher_opts;

/* Run a statement in the query language (openCypher subset). A read returns {nodes, edges, count}; a write (CREATE/MERGE/SET/REMOVE/DELETE) returns {write: true, ...change-counts}. Write-gated. (access: write) */
struct json_object *drsg_plane_cypher(drsg_client *c, const char *plane, const char *query, const drsg_plane_cypher_opts *opts, drsg_error *err);

typedef struct {
    const int64_t *limit;
    const int *semantic;
    const char *provider;
    const char *embed_model;
    const int64_t *as_of;
    const int64_t *as_of_ms;
} drsg_plane_find_opts;

/* Text (or semantic) search over the plane's nodes and edges. (access: read) */
struct json_object *drsg_plane_find(drsg_client *c, const char *plane, const char *q, const drsg_plane_find_opts *opts, drsg_error *err);

typedef struct {
    const char *label;
    const int64_t *limit;
    const double *damping;
    const int64_t *max_iters;
    const double *tolerance;
    const int64_t *src;
    const int64_t *dst;
    const char *dir;
    const char *weight;
    const int64_t *max_levels;
    const double *min_gain;
} drsg_plane_algo_opts;

/* Run a graph algorithm (pagerank | components | shortest_path | louvain) over the plane or one label subset, read-only over a single snapshot. (access: read) */
struct json_object *drsg_plane_algo(drsg_client *c, const char *plane, const char *algo, const drsg_plane_algo_opts *opts, drsg_error *err);

typedef struct {
    const char *label;
    const char *vector_prop;
    const char *keyword_prop;
    const char *metric;
    const int64_t *graph_hops;
    const double *graph_decay;
    const double *w_vector;
    const double *w_keyword;
    const double *w_graph;
    const int64_t *k;
    const int64_t *candidates;
    const char *provider;
    const char *embed_model;
} drsg_plane_hybrid_opts;

/* Hybrid retrieval: fuse vector similarity, BM25 keyword, and graph-proximity channels into one ranking. Enable a channel by naming its property (vector_prop/keyword_prop) or setting graph_hops; the vector channel embeds q server-side. (access: read) */
struct json_object *drsg_plane_hybrid(drsg_client *c, const char *plane, const char *q, const drsg_plane_hybrid_opts *opts, drsg_error *err);

typedef struct {
    const int *dry_run;
    const int64_t *max_attempts;
    const int64_t *limit;
    const char *provider;
    const char *model;
    const char *embed_provider;
    const char *embed_model;
} drsg_plane_ask_opts;

/* Natural-language query: an LLM turns the question into a read-only LogicalPlan, runs it (unless dry_run), and returns the generated plan plus result node records. With embed_provider, the model can call find_edge/find_entity embedding tools to ground the plan. Keys from the server env. (access: read) */
struct json_object *drsg_plane_ask(drsg_client *c, const char *plane, const char *question, const drsg_plane_ask_opts *opts, drsg_error *err);

/* The search indexes declared on a plane (vector + keyword), so a client can offer only the channels that actually exist. (access: read) */
struct json_object *drsg_plane_indexes(drsg_client *c, const char *plane, drsg_error *err);

typedef struct {
    const char *kind;
    const char *metric;
    const char *language;
} drsg_index_ensure_opts;

/* Declare (and build) a search index on (label, property): a keyword (BM25) or vector (embedding) index. Idempotent. (access: admin) */
struct json_object *drsg_index_ensure(drsg_client *c, const char *plane, const char *label, const char *property, const drsg_index_ensure_opts *opts, drsg_error *err);

typedef struct {
    const char *label;
    const int64_t *limit;
    const int64_t *as_of;
    const int64_t *as_of_ms;
} drsg_graph_seed_opts;

/* An initial canvas: up to `limit` nodes plus the edges induced among them. (access: read) */
struct json_object *drsg_graph_seed(drsg_client *c, const char *plane, const drsg_graph_seed_opts *opts, drsg_error *err);

typedef struct {
    const char *direction;
    const char *type;
    const int64_t *limit;
    const int64_t *as_of;
    const int64_t *as_of_ms;
} drsg_graph_expand_opts;

/* Hub-safe 1-hop neighbourhood around a node: neighbour + connecting-edge records. (access: read) */
struct json_object *drsg_graph_expand(drsg_client *c, const char *plane, int64_t id, const drsg_graph_expand_opts *opts, drsg_error *err);

typedef struct {
    const char *chat;
    const char *embed;
    const char *model;
    const char *embed_model;
    const char *source;
    const int *no_embed;
    const int *link;
} drsg_digest_run_opts;

/* Extract a node/edge proposal from text via the LLM (dry-run; spends provider credits). (access: write) */
struct json_object *drsg_digest_run(drsg_client *c, const char *plane, const char *text, const drsg_digest_run_opts *opts, drsg_error *err);

typedef struct {
    struct json_object *edges;
} drsg_digest_write_opts;

/* Write a previously-computed proposal into the plane via the bulk path (no LLM call). (access: write) */
struct json_object *drsg_digest_write(drsg_client *c, const char *plane, struct json_object *nodes, const drsg_digest_write_opts *opts, drsg_error *err);

typedef struct {
    const char *key;
    struct json_object *labels;
    struct json_object *properties;
} drsg_node_create_opts;

/* Add a node with an optional stable external key and labels. (access: write) */
struct json_object *drsg_node_create(drsg_client *c, const char *plane, const drsg_node_create_opts *opts, drsg_error *err);

typedef struct {
    const int64_t *id;
    const char *key;
    struct json_object *set;
    struct json_object *unset;
    struct json_object *labels;
} drsg_node_update_opts;

/* Patch a node: `set`/`unset` its properties, and `labels` (when present) replaces its label set. (access: write) */
struct json_object *drsg_node_update(drsg_client *c, const char *plane, const drsg_node_update_opts *opts, drsg_error *err);

typedef struct {
    const int64_t *id;
    const char *key;
} drsg_node_delete_opts;

/* Delete a node and cascade to its incident edges. (access: write) */
struct json_object *drsg_node_delete(drsg_client *c, const char *plane, const drsg_node_delete_opts *opts, drsg_error *err);

typedef struct {
    struct json_object *properties;
} drsg_edge_create_opts;

/* Add a directed edge between two existing nodes (each named by id or key). (access: write) */
struct json_object *drsg_edge_create(drsg_client *c, const char *plane, struct json_object *src, struct json_object *dst, const char *type, const drsg_edge_create_opts *opts, drsg_error *err);

typedef struct {
    struct json_object *set;
    struct json_object *unset;
    const char *type;
} drsg_edge_update_opts;

/* Patch an edge: `set`/`unset` its properties, and `type` (when present) changes its type. (access: write) */
struct json_object *drsg_edge_update(drsg_client *c, const char *plane, int64_t edge, const drsg_edge_update_opts *opts, drsg_error *err);

/* Delete one edge. (access: write) */
struct json_object *drsg_edge_delete(drsg_client *c, const char *plane, int64_t edge, drsg_error *err);

typedef struct {
    struct json_object *properties;
} drsg_plane_create_opts;

/* Make a new, empty plane. (access: admin) */
struct json_object *drsg_plane_create(drsg_client *c, const char *name, const drsg_plane_create_opts *opts, drsg_error *err);

/* Rename an existing plane. (access: admin) */
struct json_object *drsg_plane_rename(drsg_client *c, const char *plane, const char *to, drsg_error *err);

/* Replace a plane's own property map. (access: admin) */
struct json_object *drsg_plane_set_props(drsg_client *c, const char *plane, struct json_object *properties, drsg_error *err);

/* Drop a plane and everything on it (the startup plane cannot be dropped). (access: admin) */
struct json_object *drsg_plane_delete(drsg_client *c, const char *plane, drsg_error *err);

#endif /* DRSG_GENERATED_H */
