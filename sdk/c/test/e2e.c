/*
 * End-to-end test: drive a real `drsg serve` over the client.
 *
 * Reads $DRSG_BASE_URL and $DRSG_TOKEN (set by run.sh). Exits non-zero on any
 * failed assertion. run.sh starts/stops the server and skips if no binary.
 */
#include "drsg.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <unistd.h>

static int failures = 0;

static int set_fail(drsg_error *err) {
    err->code = 0;
    snprintf(err->message, sizeof err->message, "client_new returned NULL");
    return -1;
}

#define CHECK(cond, msg)                              \
    do {                                              \
        if (!(cond)) {                                \
            fprintf(stderr, "FAIL: %s\n", (msg));     \
            failures++;                               \
        }                                             \
    } while (0)

static long int_field(struct json_object *obj, const char *key) {
    struct json_object *v = NULL;
    return json_object_object_get_ex(obj, key, &v) ? json_object_get_int64(v) : -1;
}

static const char *str_field(struct json_object *obj, const char *key) {
    struct json_object *v = NULL;
    return json_object_object_get_ex(obj, key, &v) ? json_object_get_string(v) : NULL;
}

/* ---- change-feed (drsg_watch) test scaffolding --------------------------- */

struct watch_capture {
    volatile int got;
    char kind[16];
    char op[16];
};

/* Called for each change event; captures the ws-widget create and stops. */
static int on_change(struct json_object *event, void *userdata) {
    struct watch_capture *cap = userdata;
    struct json_object *changes = NULL;
    if (!json_object_object_get_ex(event, "changes", &changes)) {
        return 0;
    }
    size_t n = json_object_array_length(changes);
    for (size_t i = 0; i < n; i++) {
        struct json_object *ch = json_object_array_get_idx(changes, i);
        struct json_object *rec = NULL;
        if (json_object_object_get_ex(ch, "record", &rec)) {
            const char *key = str_field(rec, "external_key");
            if (key && strcmp(key, "ws-widget") == 0) {
                const char *kind = str_field(ch, "kind");
                const char *op = str_field(ch, "op");
                snprintf(cap->kind, sizeof cap->kind, "%s", kind ? kind : "");
                snprintf(cap->op, sizeof cap->op, "%s", op ? op : "");
                cap->got = 1;
                return 1; /* stop watching */
            }
        }
    }
    return 0;
}

struct watch_args {
    const char *base;
    const char *token;
    struct watch_capture *cap;
};

static void *watch_thread(void *arg) {
    struct watch_args *wa = arg;
    drsg_client *wc = drsg_client_new(wa->base, wa->token);
    if (wc) {
        drsg_error e;
        drsg_watch(wc, "startup", "Widget", on_change, wa->cap, &e);
        drsg_client_free(wc);
    }
    return NULL;
}

/* ---- hostile-peer and cancellation scaffolding --------------------------- */

static int on_change_never(struct json_object *event, void *userdata) {
    (void)event;
    (void)userdata;
    return 0;
}

struct cancel_args {
    const char *base;
    const char *token;
    drsg_watch_ctl *ctl;
    volatile int returned;
    int rc;
    drsg_error err;
};

/* Watches the real server with a ctl; nothing is ever committed, so only a
 * cancel from the main thread can make this return. */
static void *cancel_thread(void *arg) {
    struct cancel_args *ca = arg;
    drsg_client *wc = drsg_client_new(ca->base, ca->token);
    if (wc) {
        ca->rc = drsg_watch_cancellable(wc, "startup", NULL, on_change_never, NULL, ca->ctl, &ca->err);
        drsg_client_free(wc);
    }
    ca->returned = 1;
    return NULL;
}

/* Run drsg_watch against fake_ws.py with a token that selects its script. */
static int fake_watch(const char *token, drsg_error *err) {
    drsg_client *fc = drsg_client_new(getenv("DRSG_FAKE_URL"), token);
    if (!fc) {
        return set_fail(err);
    }
    int rc = drsg_watch(fc, "startup", NULL, on_change_never, NULL, err);
    drsg_client_free(fc);
    return rc;
}

int main(void) {
    drsg_client *c = drsg_client_new(getenv("DRSG_BASE_URL"), getenv("DRSG_TOKEN"));
    if (!c) {
        fprintf(stderr, "FAIL: client_new returned NULL\n");
        return 1;
    }
    drsg_error err;

    /* db.stats -> 0 nodes on a fresh db. */
    struct json_object *stats = drsg_db_stats(c, &err);
    CHECK(stats, "db.stats");
    CHECK(stats && int_field(stats, "nodes") == 0, "0 nodes initially");
    json_object_put(stats);

    /* node.create alice + bob. */
    struct json_object *labels = json_object_new_array();
    json_object_array_add(labels, json_object_new_string("Person"));
    drsg_node_create_opts no = {.key = "alice", .labels = labels};
    struct json_object *alice = drsg_node_create(c, "startup", &no, &err);
    CHECK(alice, "node.create alice");
    CHECK(alice && str_field(alice, "external_key")
            && strcmp(str_field(alice, "external_key"), "alice") == 0, "alice external_key");
    json_object_put(alice);

    drsg_node_create_opts nob = {.key = "bob", .labels = labels};
    struct json_object *bob = drsg_node_create(c, "startup", &nob, &err);
    CHECK(bob, "node.create bob");
    json_object_put(bob);
    json_object_put(labels);

    /* edge.create alice -KNOWS-> bob (endpoints by key). */
    struct json_object *src = json_object_new_string("alice");
    struct json_object *dst = json_object_new_string("bob");
    struct json_object *edge = drsg_edge_create(c, "startup", src, dst, "KNOWS", NULL, &err);
    CHECK(edge, "edge.create");
    CHECK(edge && str_field(edge, "type") && strcmp(str_field(edge, "type"), "KNOWS") == 0,
            "edge type KNOWS");
    json_object_put(edge);
    json_object_put(src);
    json_object_put(dst);

    /* node.update: set then unset, types preserved. */
    struct json_object *set = json_object_new_object();
    json_object_object_add(set, "age", json_object_new_int(41));
    json_object_object_add(set, "city", json_object_new_string("NYC"));
    drsg_node_update_opts uo = {.key = "alice", .set = set};
    struct json_object *upd = drsg_node_update(c, "startup", &uo, &err);
    CHECK(upd, "node.update set");
    struct json_object *props = NULL;
    if (upd) {
        json_object_object_get_ex(upd, "properties", &props);
    }
    CHECK(props && int_field(props, "age") == 41, "age == 41");
    json_object_put(upd);
    json_object_put(set);

    struct json_object *unset = json_object_new_array();
    json_object_array_add(unset, json_object_new_string("city"));
    drsg_node_update_opts uo2 = {.key = "alice", .unset = unset};
    struct json_object *upd2 = drsg_node_update(c, "startup", &uo2, &err);
    CHECK(upd2, "node.update unset");
    struct json_object *props2 = NULL, *city = NULL;
    if (upd2) {
        json_object_object_get_ex(upd2, "properties", &props2);
    }
    CHECK(props2 && !json_object_object_get_ex(props2, "city", &city), "city unset");
    json_object_put(upd2);
    json_object_put(unset);

    /* node.get reads it back. */
    drsg_node_get_opts go = {.key = "alice"};
    struct json_object *got = drsg_node_get(c, "startup", &go, &err);
    CHECK(got, "node.get alice");
    struct json_object *gp = NULL;
    if (got) {
        json_object_object_get_ex(got, "properties", &gp);
    }
    CHECK(gp && int_field(gp, "age") == 41, "get age == 41");
    json_object_put(got);

    /* node.delete cascades the edge. */
    drsg_node_delete_opts del = {.key = "alice"};
    struct json_object *deleted = drsg_node_delete(c, "startup", &del, &err);
    CHECK(deleted && int_field(deleted, "deleted") == 1, "node.delete alice");
    json_object_put(deleted);

    struct json_object *stats2 = drsg_db_stats(c, &err);
    CHECK(stats2 && int_field(stats2, "nodes") == 1 && int_field(stats2, "edges") == 0,
            "1 node, 0 edges after delete");
    json_object_put(stats2);

    /* plane admin. */
    struct json_object *plane = drsg_plane_create(c, "notes", NULL, &err);
    CHECK(plane && str_field(plane, "name") && strcmp(str_field(plane, "name"), "notes") == 0,
            "plane.create notes");
    json_object_put(plane);
    struct json_object *renamed = drsg_plane_rename(c, "notes", "archive", &err);
    CHECK(renamed && str_field(renamed, "name")
            && strcmp(str_field(renamed, "name"), "archive") == 0, "plane.rename");
    json_object_put(renamed);
    struct json_object *pdel = drsg_plane_delete(c, "archive", &err);
    CHECK(pdel && int_field(pdel, "deleted") == 1, "plane.delete");
    json_object_put(pdel);

    /* rpc.discover. */
    struct json_object *doc = drsg_rpc_discover(c, &err);
    CHECK(doc && str_field(doc, "openrpc") && strcmp(str_field(doc, "openrpc"), "1.2.6") == 0,
            "rpc.discover openrpc 1.2.6");
    json_object_put(doc);

    /* Change feed over WebSocket: watch, commit, receive. Runs the blocking
     * drsg_watch on a thread; the main thread commits a node and polls for the
     * captured event (not joining, so a broken feed can't hang the test). */
    struct watch_capture cap = {0};
    struct watch_args wa = {getenv("DRSG_BASE_URL"), getenv("DRSG_TOKEN"), &cap};
    pthread_t th;
    if (pthread_create(&th, NULL, watch_thread, &wa) == 0) {
        pthread_detach(th);
        usleep(400000); /* 400ms: let the socket connect + subscription register */

        struct json_object *wlabels = json_object_new_array();
        json_object_array_add(wlabels, json_object_new_string("Widget"));
        drsg_node_create_opts wno = {.key = "ws-widget", .labels = wlabels};
        struct json_object *wnode = drsg_node_create(c, "startup", &wno, &err);
        json_object_put(wnode);
        json_object_put(wlabels);

        for (int i = 0; i < 150 && !cap.got; i++) {
            usleep(20000); /* up to ~3s */
        }
        CHECK(cap.got, "change feed: received ws-widget event");
        CHECK(cap.got && strcmp(cap.kind, "node") == 0, "change kind == node");
        CHECK(cap.got && strcmp(cap.op, "created") == 0, "change op == created");

        /* Restore the graph (a later run's fresh db is independent, but tidy). */
        drsg_node_delete_opts wdel = {.key = "ws-widget"};
        struct json_object *wd = drsg_node_delete(c, "startup", &wdel, &err);
        json_object_put(wd);
    } else {
        CHECK(0, "pthread_create for watch");
    }

    /* Cancellation from another thread: the watch blocks on a feed that never
     * fires, and drsg_watch_ctl_cancel must make it return 0 promptly. */
    {
        struct cancel_args ca = {getenv("DRSG_BASE_URL"), getenv("DRSG_TOKEN"),
                                 drsg_watch_ctl_new(), 0, -2, {0}};
        pthread_t cth;
        CHECK(ca.ctl, "drsg_watch_ctl_new");
        if (ca.ctl && pthread_create(&cth, NULL, cancel_thread, &ca) == 0) {
            usleep(300000); /* let it connect and subscribe */
            drsg_watch_ctl_cancel(ca.ctl);
            for (int i = 0; i < 150 && !ca.returned; i++) {
                usleep(20000); /* up to ~3s */
            }
            CHECK(ca.returned, "cancel: drsg_watch_cancellable returned after cancel");
            if (ca.returned) {
                pthread_join(cth, NULL);
                CHECK(ca.rc == 0, "cancel: returns 0 (clean stop)");
            } else {
                pthread_detach(cth);
            }
        } else {
            CHECK(0, "pthread_create for cancel");
        }
        drsg_watch_ctl_free(ca.ctl);
    }

    /* Hostile peer (test/fake_ws.py): a frame header claiming 2^40 bytes must
     * be refused with a typed error rather than allocated; a 101 whose
     * Sec-WebSocket-Accept is wrong must not be trusted; a token with URL
     * metacharacters must arrive percent-encoded (the fake answers 400 to any
     * other spelling, so the watch only ends cleanly when it was escaped). */
    if (getenv("DRSG_FAKE_URL")) {
        drsg_error ferr;
        int rc = fake_watch("big", &ferr);
        CHECK(rc == -1 && ferr.code == DRSG_TRANSPORT_ERROR_CODE,
              "oversized frame: watch fails with DRSG_TRANSPORT_ERROR_CODE");
        CHECK(rc == -1 && strstr(ferr.message, "DRSG_WS_MAX_MESSAGE_BYTES") != NULL,
              "oversized frame: message names the limit");

        rc = fake_watch("bad-accept", &ferr);
        CHECK(rc == -1 && strstr(ferr.message, "Sec-WebSocket-Accept") != NULL,
              "forged handshake: Sec-WebSocket-Accept mismatch is rejected");

        rc = fake_watch("a&b=c#d", &ferr);
        CHECK(rc == 0, "token with URL metacharacters is percent-encoded");
        if (rc != 0) {
            fprintf(stderr, "  (fake peer said: %s)\n", ferr.message);
        }
    } else {
        CHECK(0, "DRSG_FAKE_URL not set (run via test/run.sh)");
    }

    /* Bad token -> auth error (-32001). */
    drsg_client *bad = drsg_client_new(getenv("DRSG_BASE_URL"), "wrong");
    struct json_object *denied = drsg_db_stats(bad, &err);
    CHECK(denied == NULL && drsg_is_auth_error(&err), "bad token -> auth error");
    CHECK(err.code == DRSG_AUTH_ERROR_CODE, "auth error code -32001");
    if (denied) {
        json_object_put(denied);
    }
    drsg_client_free(bad);

    drsg_client_free(c);

    if (failures) {
        fprintf(stderr, "%d failure(s)\n", failures);
        return 1;
    }
    printf("all C e2e checks passed\n");
    return 0;
}
