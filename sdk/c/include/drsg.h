/*
 * dr-strange C client (`drsg serve` JSON-RPC).
 *
 * The typed method surface lives in the generated drsg_generated.h /
 * drsg_generated.c (see codegen/codegen.c); this header is the hand-written
 * core. HTTP is libcurl; JSON is json-c. Every method returns a json_object the
 * caller owns (json_object_put to free).
 */
#ifndef DRSG_H
#define DRSG_H

#include <stdint.h>
#include <json-c/json.h>

#ifdef __cplusplus
extern "C" {
#endif

#define DRSG_DEFAULT_BASE_URL "http://127.0.0.1:7700"

/* JSON-RPC error code for a missing/invalid credential. */
#define DRSG_AUTH_ERROR_CODE (-32001)

typedef struct drsg_client drsg_client;

/* Filled on failure. code is the JSON-RPC error code; message is a copy. */
typedef struct {
    int code;
    char message[256];
} drsg_error;

/*
 * Create a client. base_url NULL -> DRSG_DEFAULT_BASE_URL; token NULL ->
 * the DRSG_TOKEN environment variable. Returns NULL on allocation/curl failure.
 */
drsg_client *drsg_client_new(const char *base_url, const char *token);

void drsg_client_free(drsg_client *client);

/*
 * Low-level: send one JSON-RPC call. params is borrowed (may be NULL). On
 * success returns 0 and sets *result to a new json_object the caller owns (may
 * be NULL for a JSON null result); on failure returns -1 and fills err.
 */
int drsg_call(drsg_client *client, const char *method, struct json_object *params,
              struct json_object **result, drsg_error *err);

/* Whether err is a missing/invalid credential failure (code -32001). */
static inline int drsg_is_auth_error(const drsg_error *err) {
    return err != NULL && err->code == DRSG_AUTH_ERROR_CODE;
}

/*
 * Change-feed callback (ROADMAP §5). event is a borrowed json_object
 * {plane, seq, truncated, changes:[{kind, op, id, labels?, record?}]} — do not
 * free it. Return 0 to keep watching, non-zero to stop drsg_watch.
 */
typedef int (*drsg_change_cb)(struct json_object *event, void *userdata);

/*
 * Live change-feed subscription. Opens a long-lived WebSocket to <base_url>/ws,
 * subscribes to `plane` (narrowed to node `label`, or NULL for all), and calls
 * `cb` for each committed change event. Blocks until `cb` returns non-zero, the
 * server closes the connection, or an error occurs; returns 0 on a clean
 * stop/close, -1 on error (fills err). Plain ws:// only (no TLS). Run it on a
 * dedicated thread if you need to do other work meanwhile.
 */
int drsg_watch(drsg_client *client, const char *plane, const char *label,
               drsg_change_cb cb, void *userdata, drsg_error *err);

#include "drsg_generated.h"

#ifdef __cplusplus
}
#endif

#endif /* DRSG_H */
