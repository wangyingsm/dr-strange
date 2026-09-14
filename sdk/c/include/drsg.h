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

/*
 * Error code the client itself mints for failures that never reached a
 * JSON-RPC handler: connection refused, a non-2xx status, an unparseable
 * reply, a refused or forged WebSocket handshake, an oversized change-feed
 * message. It shares the server's generic application-error code so callers
 * that only branch on "auth or not" keep working; the message says which.
 */
#define DRSG_TRANSPORT_ERROR_CODE (-32000)

/*
 * Largest WebSocket message the change feed will buffer. A frame header can
 * claim a 64-bit length; without a ceiling a hostile or confused peer could
 * make the client allocate whatever it asked for. A change event that exceeds
 * this bound ends drsg_watch with DRSG_TRANSPORT_ERROR_CODE.
 */
#define DRSG_WS_MAX_MESSAGE_BYTES ((size_t)64 * 1024 * 1024)

typedef struct drsg_client drsg_client;

/* Filled on failure. code is the JSON-RPC error code; message is a copy. */
typedef struct {
    int code;
    char message[256];
} drsg_error;

/*
 * Create a client. base_url NULL -> DRSG_DEFAULT_BASE_URL; token NULL ->
 * the DRSG_TOKEN environment variable. Returns NULL on allocation/curl failure.
 *
 * Threading: a client owns one libcurl easy handle, which libcurl forbids
 * sharing between threads, so a drsg_client must not be used by two threads
 * at once — give each thread its own. drsg_watch blocks its caller, so run it
 * on a client of its own (the one-time library initialisation and the
 * drsg_watch_ctl handle are the only parts that are safe to touch from
 * several threads).
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

/*
 * Cancellation handle for a watch running on another thread. Create one,
 * hand it to drsg_watch_cancellable, and call drsg_watch_ctl_cancel from any
 * thread to make the watch return 0 promptly (it shuts the socket down, which
 * wakes the blocked read). Cancelling before the watch starts makes it return
 * as soon as it has connected; cancelling twice is harmless. Free it only
 * after drsg_watch_cancellable has returned.
 */
typedef struct drsg_watch_ctl drsg_watch_ctl;

drsg_watch_ctl *drsg_watch_ctl_new(void);
void drsg_watch_ctl_cancel(drsg_watch_ctl *ctl);
void drsg_watch_ctl_free(drsg_watch_ctl *ctl);

/* drsg_watch with a cancellation handle (may be NULL, which is drsg_watch). */
int drsg_watch_cancellable(drsg_client *client, const char *plane, const char *label,
                           drsg_change_cb cb, void *userdata, drsg_watch_ctl *ctl,
                           drsg_error *err);

#include "drsg_generated.h"

#ifdef __cplusplus
}
#endif

#endif /* DRSG_H */
