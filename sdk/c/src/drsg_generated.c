// Code generated from crates/dr-strange-web/openrpc.json by codegen.c; DO NOT EDIT.
#include "drsg.h"

/* This OpenRPC service description. (access: read) */
struct json_object *drsg_rpc_discover(drsg_client *c, drsg_error *err) {
    struct json_object *p = NULL;
    struct json_object *result = NULL;
    int rc = drsg_call(c, "rpc.discover", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Plane/node/edge counts plus the on-disk file size when persistent. (access: read) */
struct json_object *drsg_db_stats(drsg_client *c, drsg_error *err) {
    struct json_object *p = NULL;
    struct json_object *result = NULL;
    int rc = drsg_call(c, "db.stats", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* The soft-schema catalog rolled up across every plane. (access: read) */
struct json_object *drsg_db_catalog(drsg_client *c, drsg_error *err) {
    struct json_object *p = NULL;
    struct json_object *result = NULL;
    int rc = drsg_call(c, "db.catalog", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Every plane with its id, name, counts, and own properties. (access: read) */
struct json_object *drsg_plane_list(drsg_client *c, drsg_error *err) {
    struct json_object *p = NULL;
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.list", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* One plane's soft schema (labels, property descriptions, edge types, counts). (access: read) */
struct json_object *drsg_plane_catalog(drsg_client *c, const char *plane, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.catalog", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* One node by id or external key; null if absent. (access: read) */
struct json_object *drsg_node_get(drsg_client *c, const char *plane, const drsg_node_get_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    if (opts) {
        if (opts->id) json_object_object_add(p, "id", json_object_new_int64(*opts->id));
        if (opts->key) json_object_object_add(p, "key", json_object_new_string(opts->key));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "node.get", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* 1-hop expansion as {node, edge} id pairs. (access: read) */
struct json_object *drsg_plane_neighbors(drsg_client *c, const char *plane, int64_t id, const drsg_plane_neighbors_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "id", json_object_new_int64(id));
    if (opts) {
        if (opts->direction) json_object_object_add(p, "direction", json_object_new_string(opts->direction));
        if (opts->type) json_object_object_add(p, "type", json_object_new_string(opts->type));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.neighbors", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Vector top-k over a property; returns scored node records. (access: read) */
struct json_object *drsg_plane_search(drsg_client *c, const char *plane, const char *property, struct json_object *query, const drsg_plane_search_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "property", json_object_new_string(property));
    json_object_object_add(p, "query", json_object_get(query));
    if (opts) {
        if (opts->label) json_object_object_add(p, "label", json_object_new_string(opts->label));
        if (opts->k) json_object_object_add(p, "k", json_object_new_int64(*opts->k));
        if (opts->metric) json_object_object_add(p, "metric", json_object_new_string(opts->metric));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.search", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Run a serialized logical plan verbatim; returns scored rows. (access: read) */
struct json_object *drsg_plane_query(drsg_client *c, const char *plane, struct json_object *plan, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "plan", json_object_get(plan));
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.query", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Text (or semantic) search over the plane's nodes and edges. (access: read) */
struct json_object *drsg_plane_find(drsg_client *c, const char *plane, const char *q, const drsg_plane_find_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "q", json_object_new_string(q));
    if (opts) {
        if (opts->limit) json_object_object_add(p, "limit", json_object_new_int64(*opts->limit));
        if (opts->semantic) json_object_object_add(p, "semantic", json_object_new_boolean(*opts->semantic));
        if (opts->provider) json_object_object_add(p, "provider", json_object_new_string(opts->provider));
        if (opts->embed_model) json_object_object_add(p, "embed_model", json_object_new_string(opts->embed_model));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.find", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* An initial canvas: up to `limit` nodes plus the edges induced among them. (access: read) */
struct json_object *drsg_graph_seed(drsg_client *c, const char *plane, const drsg_graph_seed_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    if (opts) {
        if (opts->label) json_object_object_add(p, "label", json_object_new_string(opts->label));
        if (opts->limit) json_object_object_add(p, "limit", json_object_new_int64(*opts->limit));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "graph.seed", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Hub-safe 1-hop neighbourhood around a node: neighbour + connecting-edge records. (access: read) */
struct json_object *drsg_graph_expand(drsg_client *c, const char *plane, int64_t id, const drsg_graph_expand_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "id", json_object_new_int64(id));
    if (opts) {
        if (opts->direction) json_object_object_add(p, "direction", json_object_new_string(opts->direction));
        if (opts->type) json_object_object_add(p, "type", json_object_new_string(opts->type));
        if (opts->limit) json_object_object_add(p, "limit", json_object_new_int64(*opts->limit));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "graph.expand", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Extract a node/edge proposal from text via the LLM (dry-run; spends provider credits). (access: write) */
struct json_object *drsg_digest_run(drsg_client *c, const char *plane, const char *text, const drsg_digest_run_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "text", json_object_new_string(text));
    if (opts) {
        if (opts->chat) json_object_object_add(p, "chat", json_object_new_string(opts->chat));
        if (opts->embed) json_object_object_add(p, "embed", json_object_new_string(opts->embed));
        if (opts->model) json_object_object_add(p, "model", json_object_new_string(opts->model));
        if (opts->embed_model) json_object_object_add(p, "embed_model", json_object_new_string(opts->embed_model));
        if (opts->source) json_object_object_add(p, "source", json_object_new_string(opts->source));
        if (opts->no_embed) json_object_object_add(p, "no_embed", json_object_new_boolean(*opts->no_embed));
        if (opts->link) json_object_object_add(p, "link", json_object_new_boolean(*opts->link));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "digest.run", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Write a previously-computed proposal into the plane via the bulk path (no LLM call). (access: write) */
struct json_object *drsg_digest_write(drsg_client *c, const char *plane, struct json_object *nodes, const drsg_digest_write_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "nodes", json_object_get(nodes));
    if (opts) {
        if (opts->edges) json_object_object_add(p, "edges", json_object_get(opts->edges));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "digest.write", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Add a node with an optional stable external key and labels. (access: write) */
struct json_object *drsg_node_create(drsg_client *c, const char *plane, const drsg_node_create_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    if (opts) {
        if (opts->key) json_object_object_add(p, "key", json_object_new_string(opts->key));
        if (opts->labels) json_object_object_add(p, "labels", json_object_get(opts->labels));
        if (opts->properties) json_object_object_add(p, "properties", json_object_get(opts->properties));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "node.create", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Patch a node's properties: `set` inserts/overwrites, `unset` removes. (access: write) */
struct json_object *drsg_node_update(drsg_client *c, const char *plane, const drsg_node_update_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    if (opts) {
        if (opts->id) json_object_object_add(p, "id", json_object_new_int64(*opts->id));
        if (opts->key) json_object_object_add(p, "key", json_object_new_string(opts->key));
        if (opts->set) json_object_object_add(p, "set", json_object_get(opts->set));
        if (opts->unset) json_object_object_add(p, "unset", json_object_get(opts->unset));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "node.update", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Delete a node and cascade to its incident edges. (access: write) */
struct json_object *drsg_node_delete(drsg_client *c, const char *plane, const drsg_node_delete_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    if (opts) {
        if (opts->id) json_object_object_add(p, "id", json_object_new_int64(*opts->id));
        if (opts->key) json_object_object_add(p, "key", json_object_new_string(opts->key));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "node.delete", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Add a directed edge between two existing nodes (each named by id or key). (access: write) */
struct json_object *drsg_edge_create(drsg_client *c, const char *plane, struct json_object *src, struct json_object *dst, const char *type, const drsg_edge_create_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "src", json_object_get(src));
    json_object_object_add(p, "dst", json_object_get(dst));
    json_object_object_add(p, "type", json_object_new_string(type));
    if (opts) {
        if (opts->properties) json_object_object_add(p, "properties", json_object_get(opts->properties));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "edge.create", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Patch an edge's properties (`set`/`unset`). (access: write) */
struct json_object *drsg_edge_update(drsg_client *c, const char *plane, int64_t edge, const drsg_edge_update_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "edge", json_object_new_int64(edge));
    if (opts) {
        if (opts->set) json_object_object_add(p, "set", json_object_get(opts->set));
        if (opts->unset) json_object_object_add(p, "unset", json_object_get(opts->unset));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "edge.update", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Delete one edge. (access: write) */
struct json_object *drsg_edge_delete(drsg_client *c, const char *plane, int64_t edge, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "edge", json_object_new_int64(edge));
    struct json_object *result = NULL;
    int rc = drsg_call(c, "edge.delete", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Make a new, empty plane. (access: admin) */
struct json_object *drsg_plane_create(drsg_client *c, const char *name, const drsg_plane_create_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "name", json_object_new_string(name));
    if (opts) {
        if (opts->properties) json_object_object_add(p, "properties", json_object_get(opts->properties));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.create", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Rename an existing plane. (access: admin) */
struct json_object *drsg_plane_rename(drsg_client *c, const char *plane, const char *to, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "to", json_object_new_string(to));
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.rename", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Replace a plane's own property map. (access: admin) */
struct json_object *drsg_plane_set_props(drsg_client *c, const char *plane, struct json_object *properties, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "properties", json_object_get(properties));
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.set_props", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Drop a plane and everything on it (the startup plane cannot be dropped). (access: admin) */
struct json_object *drsg_plane_delete(drsg_client *c, const char *plane, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.delete", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

