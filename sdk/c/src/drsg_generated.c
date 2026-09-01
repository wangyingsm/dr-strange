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

/* Installed preprocessor plugins — the same records `drsg plugin list --json` prints, so an agent reads one shape from either surface (ROADMAP §11). */
struct json_object *drsg_plugin_list(drsg_client *c, drsg_error *err) {
    struct json_object *p = NULL;
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plugin.list", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* The official plugin catalog, read from the extensions repository's catalog.json rather than compiled into this build — a plugin release needs no drsg release. Entries this build cannot run are returned tagged with why, not filtered out. Join against plugin.list to mark each installed/upgradable/absent. Cached for an hour; stale:true means the fetch failed and this is the last copy the store kept. */
struct json_object *drsg_plugin_catalog(drsg_client *c, drsg_error *err) {
    struct json_object *p = NULL;
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plugin.catalog", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Download, validate, hash-pin and store a plugin from an http(s) URL. Write-gated; the URL passes the same resolved-address network policy as every other fetch. Server-local paths are deliberately not accepted over RPC. */
struct json_object *drsg_plugin_install(drsg_client *c, const char *url, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "url", json_object_new_string(url));
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plugin.install", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Uninstall a plugin by name. Write-gated. */
struct json_object *drsg_plugin_remove(drsg_client *c, const char *name, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "name", json_object_new_string(name));
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plugin.remove", p, &result, err);
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

/* Embed every node in a plane (incremental by meaning — unchanged texts are skipped) and ensure a vector index on `embedding` per label. Same engine as `drsg vectorize`; the provider key comes from the server's environment. */
struct json_object *drsg_plane_vectorize(drsg_client *c, const char *plane, const drsg_plane_vectorize_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    if (opts) {
        if (opts->embed) json_object_object_add(p, "embed", json_object_new_string(opts->embed));
        if (opts->embed_model) json_object_object_add(p, "embed_model", json_object_new_string(opts->embed_model));
        if (opts->metric) json_object_object_add(p, "metric", json_object_new_string(opts->metric));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.vectorize", p, &result, err);
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
        if (opts->lean) json_object_object_add(p, "lean", json_object_new_boolean(*opts->lean));
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
        if (opts->as_of) json_object_object_add(p, "as_of", json_object_new_int64(*opts->as_of));
        if (opts->as_of_ms) json_object_object_add(p, "as_of_ms", json_object_new_int64(*opts->as_of_ms));
        if (opts->hydrate) json_object_object_add(p, "hydrate", json_object_new_boolean(*opts->hydrate));
        if (opts->lean) json_object_object_add(p, "lean", json_object_new_boolean(*opts->lean));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.neighbors", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Time-travel window: oldest and latest commit sequences a read can be pinned to (native backend only). (access: read) */
struct json_object *drsg_plane_history(drsg_client *c, drsg_error *err) {
    struct json_object *p = NULL;
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.history", p, &result, err);
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
struct json_object *drsg_plane_query(drsg_client *c, const char *plane, struct json_object *plan, const drsg_plane_query_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "plan", json_object_get(plan));
    if (opts) {
        if (opts->as_of) json_object_object_add(p, "as_of", json_object_new_int64(*opts->as_of));
        if (opts->as_of_ms) json_object_object_add(p, "as_of_ms", json_object_new_int64(*opts->as_of_ms));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.query", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Run a statement in the query language (openCypher subset). A read returns {nodes, edges, count}; a write (CREATE/MERGE/SET/REMOVE/DELETE) returns {write: true, ...change-counts}. Write-gated. (access: write) */
struct json_object *drsg_plane_cypher(drsg_client *c, const char *plane, const char *query, const drsg_plane_cypher_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "query", json_object_new_string(query));
    if (opts) {
        if (opts->embed) json_object_object_add(p, "embed", json_object_new_string(opts->embed));
        if (opts->params) json_object_object_add(p, "params", json_object_get(opts->params));
        if (opts->lean) json_object_object_add(p, "lean", json_object_new_boolean(*opts->lean));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.cypher", p, &result, err);
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
        if (opts->as_of) json_object_object_add(p, "as_of", json_object_new_int64(*opts->as_of));
        if (opts->as_of_ms) json_object_object_add(p, "as_of_ms", json_object_new_int64(*opts->as_of_ms));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.find", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Run a graph algorithm (pagerank | components | shortest_path | louvain) over the plane or one label subset, read-only over a single snapshot. (access: read) */
struct json_object *drsg_plane_algo(drsg_client *c, const char *plane, const char *algo, const drsg_plane_algo_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "algo", json_object_new_string(algo));
    if (opts) {
        if (opts->label) json_object_object_add(p, "label", json_object_new_string(opts->label));
        if (opts->limit) json_object_object_add(p, "limit", json_object_new_int64(*opts->limit));
        if (opts->damping) json_object_object_add(p, "damping", json_object_new_double(*opts->damping));
        if (opts->max_iters) json_object_object_add(p, "max_iters", json_object_new_int64(*opts->max_iters));
        if (opts->tolerance) json_object_object_add(p, "tolerance", json_object_new_double(*opts->tolerance));
        if (opts->src) json_object_object_add(p, "src", json_object_new_int64(*opts->src));
        if (opts->dst) json_object_object_add(p, "dst", json_object_new_int64(*opts->dst));
        if (opts->dir) json_object_object_add(p, "dir", json_object_new_string(opts->dir));
        if (opts->weight) json_object_object_add(p, "weight", json_object_new_string(opts->weight));
        if (opts->max_levels) json_object_object_add(p, "max_levels", json_object_new_int64(*opts->max_levels));
        if (opts->min_gain) json_object_object_add(p, "min_gain", json_object_new_double(*opts->min_gain));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.algo", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Hybrid retrieval: fuse vector similarity, BM25 keyword, and graph-proximity channels into one ranking. Enable a channel by naming its property (vector_prop/keyword_prop) or setting graph_hops; the vector channel embeds q server-side. (access: read) */
struct json_object *drsg_plane_hybrid(drsg_client *c, const char *plane, const char *q, const drsg_plane_hybrid_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "q", json_object_new_string(q));
    if (opts) {
        if (opts->label) json_object_object_add(p, "label", json_object_new_string(opts->label));
        if (opts->vector_prop) json_object_object_add(p, "vector_prop", json_object_new_string(opts->vector_prop));
        if (opts->keyword_prop) json_object_object_add(p, "keyword_prop", json_object_new_string(opts->keyword_prop));
        if (opts->metric) json_object_object_add(p, "metric", json_object_new_string(opts->metric));
        if (opts->graph_hops) json_object_object_add(p, "graph_hops", json_object_new_int64(*opts->graph_hops));
        if (opts->graph_decay) json_object_object_add(p, "graph_decay", json_object_new_double(*opts->graph_decay));
        if (opts->w_vector) json_object_object_add(p, "w_vector", json_object_new_double(*opts->w_vector));
        if (opts->w_keyword) json_object_object_add(p, "w_keyword", json_object_new_double(*opts->w_keyword));
        if (opts->w_graph) json_object_object_add(p, "w_graph", json_object_new_double(*opts->w_graph));
        if (opts->k) json_object_object_add(p, "k", json_object_new_int64(*opts->k));
        if (opts->candidates) json_object_object_add(p, "candidates", json_object_new_int64(*opts->candidates));
        if (opts->provider) json_object_object_add(p, "provider", json_object_new_string(opts->provider));
        if (opts->embed_model) json_object_object_add(p, "embed_model", json_object_new_string(opts->embed_model));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.hybrid", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Natural-language query: an LLM turns the question into a read-only LogicalPlan, runs it (unless dry_run), and returns the generated plan plus result node records. With embed_provider, the model can call find_edge/find_entity embedding tools to ground the plan. Keys from the server env. (access: read) */
struct json_object *drsg_plane_ask(drsg_client *c, const char *plane, const char *question, const drsg_plane_ask_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "question", json_object_new_string(question));
    if (opts) {
        if (opts->dry_run) json_object_object_add(p, "dry_run", json_object_new_boolean(*opts->dry_run));
        if (opts->max_attempts) json_object_object_add(p, "max_attempts", json_object_new_int64(*opts->max_attempts));
        if (opts->limit) json_object_object_add(p, "limit", json_object_new_int64(*opts->limit));
        if (opts->provider) json_object_object_add(p, "provider", json_object_new_string(opts->provider));
        if (opts->model) json_object_object_add(p, "model", json_object_new_string(opts->model));
        if (opts->embed_provider) json_object_object_add(p, "embed_provider", json_object_new_string(opts->embed_provider));
        if (opts->embed_model) json_object_object_add(p, "embed_model", json_object_new_string(opts->embed_model));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.ask", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* The search indexes declared on a plane (vector + keyword), so a client can offer only the channels that actually exist. (access: read) */
struct json_object *drsg_plane_indexes(drsg_client *c, const char *plane, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    struct json_object *result = NULL;
    int rc = drsg_call(c, "plane.indexes", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Declare (and build) a search index on (label, property): a keyword (BM25) or vector (embedding) index. Idempotent. (access: admin) */
struct json_object *drsg_index_ensure(drsg_client *c, const char *plane, const char *label, const char *property, const drsg_index_ensure_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "label", json_object_new_string(label));
    json_object_object_add(p, "property", json_object_new_string(property));
    if (opts) {
        if (opts->kind) json_object_object_add(p, "kind", json_object_new_string(opts->kind));
        if (opts->metric) json_object_object_add(p, "metric", json_object_new_string(opts->metric));
        if (opts->language) json_object_object_add(p, "language", json_object_new_string(opts->language));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "index.ensure", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* An initial canvas of nodes plus induced edges. `order` seeds the highest-ranked nodes rather than the first the scan reaches — a legible skeleton instead of an arbitrary sample — and returns the scores alongside, so a caller can size or weight by importance without a second call. (access: read) */
struct json_object *drsg_graph_seed(drsg_client *c, const char *plane, const drsg_graph_seed_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    if (opts) {
        if (opts->label) json_object_object_add(p, "label", json_object_new_string(opts->label));
        if (opts->limit) json_object_object_add(p, "limit", json_object_new_int64(*opts->limit));
        if (opts->order) json_object_object_add(p, "order", json_object_new_string(opts->order));
        if (opts->as_of) json_object_object_add(p, "as_of", json_object_new_int64(*opts->as_of));
        if (opts->as_of_ms) json_object_object_add(p, "as_of_ms", json_object_new_int64(*opts->as_of_ms));
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
        if (opts->as_of) json_object_object_add(p, "as_of", json_object_new_int64(*opts->as_of));
        if (opts->as_of_ms) json_object_object_add(p, "as_of_ms", json_object_new_int64(*opts->as_of_ms));
    }
    struct json_object *result = NULL;
    int rc = drsg_call(c, "graph.expand", p, &result, err);
    if (p) json_object_put(p);
    return rc == 0 ? result : NULL;
}

/* Extract a node/edge proposal from text via the LLM (dry-run; spends provider credits). `mode` sets how much clean-up follows the extraction: `coarse` reconciles the label and edge-type vocabularies, `fine` (the default) also merges entities that name the same thing, `super` also re-reads every entity against all the passages mentioning it — most accurate, and ~15x the input token usage. (access: write) */
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
        if (opts->concurrency) json_object_object_add(p, "concurrency", json_object_new_int64(*opts->concurrency));
        if (opts->chunk_chars) json_object_object_add(p, "chunk_chars", json_object_new_int64(*opts->chunk_chars));
        if (opts->mode) json_object_object_add(p, "mode", json_object_new_string(opts->mode));
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

/* Patch a node: `set`/`unset` its properties, and `labels` (when present) replaces its label set. (access: write) */
struct json_object *drsg_node_update(drsg_client *c, const char *plane, const drsg_node_update_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    if (opts) {
        if (opts->id) json_object_object_add(p, "id", json_object_new_int64(*opts->id));
        if (opts->key) json_object_object_add(p, "key", json_object_new_string(opts->key));
        if (opts->set) json_object_object_add(p, "set", json_object_get(opts->set));
        if (opts->unset) json_object_object_add(p, "unset", json_object_get(opts->unset));
        if (opts->labels) json_object_object_add(p, "labels", json_object_get(opts->labels));
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

/* Patch an edge: `set`/`unset` its properties, and `type` (when present) changes its type. (access: write) */
struct json_object *drsg_edge_update(drsg_client *c, const char *plane, int64_t edge, const drsg_edge_update_opts *opts, drsg_error *err) {
    struct json_object *p = json_object_new_object();
    json_object_object_add(p, "plane", json_object_new_string(plane));
    json_object_object_add(p, "edge", json_object_new_int64(edge));
    if (opts) {
        if (opts->set) json_object_object_add(p, "set", json_object_get(opts->set));
        if (opts->unset) json_object_object_add(p, "unset", json_object_get(opts->unset));
        if (opts->type) json_object_object_add(p, "type", json_object_new_string(opts->type));
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

