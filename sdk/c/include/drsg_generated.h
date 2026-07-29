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
} drsg_plane_neighbors_opts;

/* 1-hop expansion as {node, edge} id pairs. (access: read) */
struct json_object *drsg_plane_neighbors(drsg_client *c, const char *plane, int64_t id, const drsg_plane_neighbors_opts *opts, drsg_error *err);

typedef struct {
    const char *label;
    const int64_t *k;
    const char *metric;
} drsg_plane_search_opts;

/* Vector top-k over a property; returns scored node records. (access: read) */
struct json_object *drsg_plane_search(drsg_client *c, const char *plane, const char *property, struct json_object *query, const drsg_plane_search_opts *opts, drsg_error *err);

/* Run a serialized logical plan verbatim; returns scored rows. (access: read) */
struct json_object *drsg_plane_query(drsg_client *c, const char *plane, struct json_object *plan, drsg_error *err);

typedef struct {
    const int64_t *limit;
    const int *semantic;
    const char *provider;
    const char *embed_model;
} drsg_plane_find_opts;

/* Text (or semantic) search over the plane's nodes and edges. (access: read) */
struct json_object *drsg_plane_find(drsg_client *c, const char *plane, const char *q, const drsg_plane_find_opts *opts, drsg_error *err);

typedef struct {
    const char *label;
    const int64_t *limit;
} drsg_graph_seed_opts;

/* An initial canvas: up to `limit` nodes plus the edges induced among them. (access: read) */
struct json_object *drsg_graph_seed(drsg_client *c, const char *plane, const drsg_graph_seed_opts *opts, drsg_error *err);

typedef struct {
    const char *direction;
    const char *type;
    const int64_t *limit;
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
} drsg_node_update_opts;

/* Patch a node's properties: `set` inserts/overwrites, `unset` removes. (access: write) */
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
} drsg_edge_update_opts;

/* Patch an edge's properties (`set`/`unset`). (access: write) */
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
