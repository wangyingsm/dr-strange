/* Hand-written JSON-RPC transport for the dr-strange C client. */
#include "drsg.h"

#include <curl/curl.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netdb.h>

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

/* ---- live change feed over WebSocket (ROADMAP §5) ------------------------ */
/*
 * libcurl < 7.86 has no WebSocket API, so this is a hand-rolled RFC 6455
 * text-frame client over a POSIX socket: client frames are masked, server
 * frames are de-fragmented, and pings are answered. Plain ws:// only.
 */

struct ws_rd {
    int fd;
    unsigned char *hold; /* bytes read past the handshake, consumed first */
    size_t hold_len, hold_pos;
};

/* Read exactly n bytes into dst (draining the handshake leftover first). */
static int ws_read_exact(struct ws_rd *rd, unsigned char *dst, size_t n) {
    size_t got = 0;
    while (got < n) {
        if (rd->hold_pos < rd->hold_len) {
            size_t avail = rd->hold_len - rd->hold_pos;
            size_t take = avail < n - got ? avail : n - got;
            memcpy(dst + got, rd->hold + rd->hold_pos, take);
            rd->hold_pos += take;
            got += take;
            continue;
        }
        ssize_t r = recv(rd->fd, dst + got, n - got, 0);
        if (r <= 0) {
            return -1;
        }
        got += (size_t)r;
    }
    return 0;
}

/* Send one frame; client frames are always masked (RFC 6455 §5.3). */
static int ws_send_frame(int fd, unsigned char opcode, const unsigned char *payload, size_t n) {
    unsigned char header[14];
    size_t h = 0;
    header[h++] = (unsigned char)(0x80 | opcode); /* FIN + opcode */
    if (n < 126) {
        header[h++] = (unsigned char)(0x80 | n);
    } else if (n < 65536) {
        header[h++] = 0x80 | 126;
        header[h++] = (unsigned char)(n >> 8);
        header[h++] = (unsigned char)(n & 0xFF);
    } else {
        header[h++] = 0x80 | 127;
        for (int i = 7; i >= 0; i--) {
            header[h++] = (unsigned char)((uint64_t)n >> (i * 8) & 0xFF);
        }
    }
    /* A fixed mask is fine for a client that talks only to our own server. */
    const unsigned char mask[4] = {0x37, 0xFA, 0x21, 0x3D};
    memcpy(header + h, mask, 4);
    h += 4;
    if (send(fd, header, h, 0) != (ssize_t)h) {
        return -1;
    }
    if (n == 0) {
        return 0;
    }
    unsigned char *masked = malloc(n);
    if (!masked) {
        return -1;
    }
    for (size_t i = 0; i < n; i++) {
        masked[i] = payload[i] ^ mask[i % 4];
    }
    int rc = send(fd, masked, n, 0) == (ssize_t)n ? 0 : -1;
    free(masked);
    return rc;
}

/* Next complete text message (malloc'd, NUL-terminated), or NULL on close. */
static char *ws_read_message(struct ws_rd *rd) {
    unsigned char *msg = NULL;
    size_t msg_len = 0;
    for (;;) {
        unsigned char head[2];
        if (ws_read_exact(rd, head, 2)) {
            free(msg);
            return NULL;
        }
        int fin = head[0] & 0x80;
        int opcode = head[0] & 0x0F;
        int masked = head[1] & 0x80;
        uint64_t len = head[1] & 0x7F;
        if (len == 126) {
            unsigned char e[2];
            if (ws_read_exact(rd, e, 2)) {
                free(msg);
                return NULL;
            }
            len = (uint64_t)e[0] << 8 | e[1];
        } else if (len == 127) {
            unsigned char e[8];
            if (ws_read_exact(rd, e, 8)) {
                free(msg);
                return NULL;
            }
            len = 0;
            for (int i = 0; i < 8; i++) {
                len = len << 8 | e[i];
            }
        }
        unsigned char mask[4];
        if (masked && ws_read_exact(rd, mask, 4)) {
            free(msg);
            return NULL;
        }
        unsigned char *data = NULL;
        if (len) {
            data = malloc(len);
            if (!data || ws_read_exact(rd, data, len)) {
                free(data);
                free(msg);
                return NULL;
            }
            if (masked) {
                for (uint64_t i = 0; i < len; i++) {
                    data[i] ^= mask[i % 4];
                }
            }
        }
        if (opcode == 0x8) { /* close */
            free(data);
            free(msg);
            return NULL;
        }
        if (opcode == 0x9) { /* ping -> pong */
            ws_send_frame(rd->fd, 0xA, data, len);
            free(data);
            continue;
        }
        if (opcode == 0xA) { /* pong */
            free(data);
            continue;
        }
        /* text (0x1) or continuation (0x0): accumulate until FIN */
        unsigned char *grown = realloc(msg, msg_len + len + 1);
        if (!grown) {
            free(data);
            free(msg);
            return NULL;
        }
        msg = grown;
        if (len) {
            memcpy(msg + msg_len, data, len);
        }
        msg_len += len;
        free(data);
        if (fin) {
            msg[msg_len] = '\0';
            return (char *)msg;
        }
    }
}

/* Case-sensitive substring search over a byte range (no _GNU_SOURCE memmem). */
static int ws_contains(const unsigned char *hay, size_t n, const char *needle) {
    size_t m = strlen(needle);
    if (m == 0 || n < m) {
        return 0;
    }
    for (size_t i = 0; i + m <= n; i++) {
        if (memcmp(hay + i, needle, m) == 0) {
            return 1;
        }
    }
    return 0;
}

/* Open a ws:// connection to <base_url>/ws and complete the handshake. */
static int ws_connect(const char *base_url, const char *token, struct ws_rd *rd, drsg_error *err) {
    if (strncmp(base_url, "http://", 7) != 0) {
        return set_err(err, -32000, "drsg_watch supports ws:// (http://) endpoints only");
    }
    const char *hostport = base_url + 7;
    size_t hp_len = strcspn(hostport, "/"); /* stop at the path, if any */
    char host[256], port[16] = "80";
    const char *colon = memchr(hostport, ':', hp_len);
    if (colon) {
        size_t host_len = (size_t)(colon - hostport);
        size_t port_len = hp_len - host_len - 1;
        if (host_len >= sizeof host || port_len >= sizeof port) {
            return set_err(err, -32000, "endpoint too long");
        }
        memcpy(host, hostport, host_len);
        host[host_len] = '\0';
        memcpy(port, colon + 1, port_len);
        port[port_len] = '\0';
    } else {
        if (hp_len >= sizeof host) {
            return set_err(err, -32000, "endpoint too long");
        }
        memcpy(host, hostport, hp_len);
        host[hp_len] = '\0';
    }

    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(host, port, &hints, &res) != 0) {
        return set_err(err, -32000, "connection failed: cannot resolve host");
    }
    int fd = -1;
    for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
        fd = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
        if (fd < 0) {
            continue;
        }
        if (connect(fd, ai->ai_addr, ai->ai_addrlen) == 0) {
            break;
        }
        close(fd);
        fd = -1;
    }
    freeaddrinfo(res);
    if (fd < 0) {
        return set_err(err, -32000, "connection failed");
    }

    /* Fixed Sec-WebSocket-Key: we don't validate the server's Accept, so any
     * valid base64 nonce works (RFC 6455 §4.1). Tokens are assumed URL-safe. */
    char req[1024];
    int rn = snprintf(req, sizeof req,
                      "GET /ws%s%s HTTP/1.1\r\n"
                      "Host: %s:%s\r\n"
                      "Upgrade: websocket\r\n"
                      "Connection: Upgrade\r\n"
                      "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                      "Sec-WebSocket-Version: 13\r\n\r\n",
                      token ? "?token=" : "", token ? token : "", host, port);
    if (rn < 0 || rn >= (int)sizeof req || send(fd, req, (size_t)rn, 0) != rn) {
        close(fd);
        return set_err(err, -32000, "handshake write failed");
    }

    unsigned char buf[8192];
    size_t len = 0;
    long sep = -1;
    while (len < sizeof buf) {
        ssize_t r = recv(fd, buf + len, sizeof buf - len, 0);
        if (r <= 0) {
            close(fd);
            return set_err(err, -32000, "handshake read failed");
        }
        len += (size_t)r;
        for (size_t i = 0; i + 4 <= len; i++) {
            if (memcmp(buf + i, "\r\n\r\n", 4) == 0) {
                sep = (long)i;
                break;
            }
        }
        if (sep >= 0) {
            break;
        }
    }
    if (sep < 0 || !ws_contains(buf, (size_t)sep, " 101 ")) {
        close(fd);
        return set_err(err, -32000, "websocket upgrade refused");
    }
    size_t hdr_len = (size_t)sep + 4;
    size_t left = len - hdr_len;
    rd->fd = fd;
    rd->hold = malloc(left ? left : 1);
    if (!rd->hold) {
        close(fd);
        return set_err(err, -32000, "out of memory");
    }
    memcpy(rd->hold, buf + hdr_len, left);
    rd->hold_len = left;
    rd->hold_pos = 0;
    return 0;
}

static void ws_close(struct ws_rd *rd) {
    if (rd->fd >= 0) {
        ws_send_frame(rd->fd, 0x8, NULL, 0); /* best-effort close */
        close(rd->fd);
        rd->fd = -1;
    }
    free(rd->hold);
    rd->hold = NULL;
}

int drsg_watch(drsg_client *c, const char *plane, const char *label,
               drsg_change_cb cb, void *userdata, drsg_error *err) {
    if (err) {
        err->code = 0;
        err->message[0] = '\0';
    }
    if (!c || !plane || !cb) {
        return set_err(err, -32000, "drsg_watch: client, plane, and cb are required");
    }

    struct ws_rd rd = {.fd = -1};
    if (ws_connect(c->base_url, c->token, &rd, err)) {
        return -1;
    }

    struct json_object *sub = json_object_new_object();
    json_object_object_add(sub, "plane", json_object_new_string(plane));
    if (label) {
        json_object_object_add(sub, "label", json_object_new_string(label));
    }
    struct json_object *req = json_object_new_object();
    json_object_object_add(req, "jsonrpc", json_object_new_string("2.0"));
    json_object_object_add(req, "method", json_object_new_string("plane.watch"));
    json_object_object_add(req, "params", sub);
    const char *reqstr = json_object_to_json_string(req);
    int send_rc = ws_send_frame(rd.fd, 0x1, (const unsigned char *)reqstr, strlen(reqstr));
    json_object_put(req);
    if (send_rc) {
        ws_close(&rd);
        return set_err(err, -32000, "websocket subscribe failed");
    }

    for (;;) {
        char *text = ws_read_message(&rd);
        if (!text) {
            break; /* clean close */
        }
        struct json_object *msg = json_tokener_parse(text);
        free(text);
        if (!msg) {
            continue;
        }
        struct json_object *method = NULL, *params = NULL;
        if (json_object_object_get_ex(msg, "method", &method)
            && strcmp(json_object_get_string(method), "plane.change") == 0
            && json_object_object_get_ex(msg, "params", &params)) {
            int stop = cb(params, userdata);
            if (stop) {
                json_object_put(msg);
                break;
            }
        }
        json_object_put(msg);
    }

    ws_close(&rd);
    return 0;
}
