/* Hand-written JSON-RPC transport for the dr-strange C client. */
#include "drsg.h"

#include <curl/curl.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

struct drsg_client {
    char *base_url;
    char *token;
    CURL *curl;
    long next_id;
};

static void ensure_global_init(void) {
    static int done = 0;
    if (!done) {
        curl_global_init(CURL_GLOBAL_DEFAULT);
        done = 1;
    }
}

drsg_client *drsg_client_new(const char *base_url, const char *token) {
    ensure_global_init();
    drsg_client *c = calloc(1, sizeof *c);
    if (!c) {
        return NULL;
    }

    const char *b = base_url ? base_url : DRSG_DEFAULT_BASE_URL;
    size_t n = strlen(b);
    while (n > 0 && b[n - 1] == '/') {
        n--;
    }
    c->base_url = strndup(b, n);

    const char *t = token ? token : getenv("DRSG_TOKEN");
    c->token = t ? strdup(t) : NULL;

    c->curl = curl_easy_init();
    c->next_id = 0;

    if (!c->base_url || !c->curl) {
        drsg_client_free(c);
        return NULL;
    }
    return c;
}

void drsg_client_free(drsg_client *c) {
    if (!c) {
        return;
    }
    if (c->curl) {
        curl_easy_cleanup(c->curl);
    }
    free(c->base_url);
    free(c->token);
    free(c);
}

struct buf {
    char *data;
    size_t len;
};

static size_t on_write(char *ptr, size_t size, size_t nmemb, void *ud) {
    size_t add = size * nmemb;
    struct buf *b = ud;
    char *grown = realloc(b->data, b->len + add + 1);
    if (!grown) {
        return 0;
    }
    b->data = grown;
    memcpy(b->data + b->len, ptr, add);
    b->len += add;
    b->data[b->len] = '\0';
    return add;
}

static int set_err(drsg_error *err, int code, const char *msg) {
    if (err) {
        err->code = code;
        snprintf(err->message, sizeof err->message, "%s", msg ? msg : "");
    }
    return -1;
}

int drsg_call(drsg_client *c, const char *method, struct json_object *params,
              struct json_object **result, drsg_error *err) {
    if (err) {
        err->code = 0;
        err->message[0] = '\0';
    }
    if (result) {
        *result = NULL;
    }

    struct json_object *req = json_object_new_object();
    json_object_object_add(req, "jsonrpc", json_object_new_string("2.0"));
    json_object_object_add(req, "method", json_object_new_string(method));
    json_object_object_add(req, "id", json_object_new_int64(++c->next_id));
    if (params) {
        json_object_object_add(req, "params", json_object_get(params));
    }
    const char *body = json_object_to_json_string_ext(req, JSON_C_TO_STRING_PLAIN);

    char *url = malloc(strlen(c->base_url) + 5);
    if (!url) {
        json_object_put(req);
        return set_err(err, -32000, "out of memory");
    }
    sprintf(url, "%s/rpc", c->base_url);

    struct curl_slist *hdr = NULL;
    hdr = curl_slist_append(hdr, "Content-Type: application/json");
    char *authz = NULL;
    if (c->token && c->token[0]) {
        size_t an = strlen(c->token) + 24;
        authz = malloc(an);
        if (authz) {
            snprintf(authz, an, "Authorization: Bearer %s", c->token);
            hdr = curl_slist_append(hdr, authz);
        }
    }

    struct buf resp = {0};
    CURL *h = c->curl;
    curl_easy_reset(h);
    curl_easy_setopt(h, CURLOPT_URL, url);
    curl_easy_setopt(h, CURLOPT_HTTPHEADER, hdr);
    curl_easy_setopt(h, CURLOPT_POST, 1L);
    curl_easy_setopt(h, CURLOPT_COPYPOSTFIELDS, body);
    curl_easy_setopt(h, CURLOPT_WRITEFUNCTION, on_write);
    curl_easy_setopt(h, CURLOPT_WRITEDATA, &resp);
    curl_easy_setopt(h, CURLOPT_TIMEOUT, 30L);

    CURLcode rc = curl_easy_perform(h);
    long status = 0;
    curl_easy_getinfo(h, CURLINFO_RESPONSE_CODE, &status);

    curl_slist_free_all(hdr);
    free(authz);
    free(url);
    json_object_put(req);

    if (rc != CURLE_OK) {
        free(resp.data);
        char m[128];
        snprintf(m, sizeof m, "connection failed: %s", curl_easy_strerror(rc));
        return set_err(err, -32000, m);
    }
    if (status / 100 != 2) {
        free(resp.data);
        char m[64];
        snprintf(m, sizeof m, "HTTP %ld", status);
        return set_err(err, -32000, m);
    }

    struct json_object *msg = json_tokener_parse(resp.data ? resp.data : "");
    free(resp.data);
    if (!msg) {
        return set_err(err, -32000, "decode response failed");
    }

    struct json_object *jerr = NULL;
    if (json_object_object_get_ex(msg, "error", &jerr)
            && !json_object_is_type(jerr, json_type_null)) {
        struct json_object *jc = NULL, *jm = NULL;
        int code = json_object_object_get_ex(jerr, "code", &jc)
                ? json_object_get_int(jc) : -32000;
        const char *m = json_object_object_get_ex(jerr, "message", &jm)
                ? json_object_get_string(jm) : "error";
        set_err(err, code, m);
        json_object_put(msg);
        return -1;
    }

    struct json_object *res = NULL;
    if (result && json_object_object_get_ex(msg, "result", &res)) {
        *result = res ? json_object_get(res) : NULL;
    }
    json_object_put(msg);
    return 0;
}
