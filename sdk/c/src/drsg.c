/* Hand-written JSON-RPC transport for the dr-strange C client. */
#include "drsg.h"

#include <curl/curl.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <time.h>
#include <pthread.h>
#include <sys/socket.h>
#include <netdb.h>

/* A peer that has hung up must fail our send(), not SIGPIPE the process. */
#ifndef MSG_NOSIGNAL
#define MSG_NOSIGNAL 0
#endif

struct drsg_client {
    char *base_url;
    char *token;
    CURL *curl;
    long next_id;
};

/*
 * curl_global_init is not thread-safe and must run exactly once per process;
 * two threads creating their first client concurrently used to race a plain
 * static flag. pthread_once serialises them.
 */
static pthread_once_t global_init_once = PTHREAD_ONCE_INIT;

static void global_init(void) {
    curl_global_init(CURL_GLOBAL_DEFAULT);
}

static void ensure_global_init(void) {
    pthread_once(&global_init_once, global_init);
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

/*
 * Fill dst with n unpredictable bytes: /dev/urandom when available, otherwise
 * a xorshift stream seeded from the clock and pid. The mask key and handshake
 * nonce only need to be unpredictable to intermediaries (RFC 6455 §10.3), so
 * the fallback is acceptable where the device is missing.
 */
static void random_bytes(unsigned char *dst, size_t n) {
    FILE *f = fopen("/dev/urandom", "rb");
    if (f) {
        size_t got = fread(dst, 1, n, f);
        fclose(f);
        if (got == n) {
            return;
        }
    }
    static uint64_t state;
    if (state == 0) {
        struct timespec ts;
        clock_gettime(CLOCK_REALTIME, &ts);
        state = (uint64_t)ts.tv_sec * 1000000007ULL ^ (uint64_t)ts.tv_nsec
                ^ (uint64_t)getpid() << 32 ^ (uintptr_t)dst;
        if (state == 0) {
            state = 0x9E3779B97F4A7C15ULL;
        }
    }
    for (size_t i = 0; i < n; i++) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        dst[i] = (unsigned char)(state >> 24);
    }
}

/* SHA-1 (RFC 3174) of one buffer; only needed for Sec-WebSocket-Accept. */
static void sha1(const unsigned char *data, size_t n, unsigned char out[20]) {
    uint32_t h[5] = {0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0};
    uint64_t bits = (uint64_t)n * 8;
    size_t padded = ((n + 8) / 64 + 1) * 64;
    unsigned char *buf = calloc(padded, 1);
    if (!buf) {
        memset(out, 0, 20);
        return;
    }
    memcpy(buf, data, n);
    buf[n] = 0x80;
    for (int i = 0; i < 8; i++) {
        buf[padded - 1 - i] = (unsigned char)(bits >> (8 * i));
    }
    for (size_t off = 0; off < padded; off += 64) {
        uint32_t w[80];
        for (int i = 0; i < 16; i++) {
            const unsigned char *p = buf + off + i * 4;
            w[i] = (uint32_t)p[0] << 24 | (uint32_t)p[1] << 16 | (uint32_t)p[2] << 8 | p[3];
        }
        for (int i = 16; i < 80; i++) {
            uint32_t x = w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16];
            w[i] = x << 1 | x >> 31;
        }
        uint32_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4];
        for (int i = 0; i < 80; i++) {
            uint32_t f, k;
            if (i < 20) {
                f = (b & c) | (~b & d);
                k = 0x5A827999;
            } else if (i < 40) {
                f = b ^ c ^ d;
                k = 0x6ED9EBA1;
            } else if (i < 60) {
                f = (b & c) | (b & d) | (c & d);
                k = 0x8F1BBCDC;
            } else {
                f = b ^ c ^ d;
                k = 0xCA62C1D6;
            }
            uint32_t t = (a << 5 | a >> 27) + f + e + k + w[i];
            e = d;
            d = c;
            c = b << 30 | b >> 2;
            b = a;
            a = t;
        }
        h[0] += a;
        h[1] += b;
        h[2] += c;
        h[3] += d;
        h[4] += e;
    }
    free(buf);
    for (int i = 0; i < 5; i++) {
        out[i * 4] = (unsigned char)(h[i] >> 24);
        out[i * 4 + 1] = (unsigned char)(h[i] >> 16);
        out[i * 4 + 2] = (unsigned char)(h[i] >> 8);
        out[i * 4 + 3] = (unsigned char)h[i];
    }
}

/* Standard base64 with padding; out must hold 4 * ceil(n / 3) + 1 bytes. */
static void base64_encode(const unsigned char *in, size_t n, char *out) {
    static const char tbl[] =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    size_t o = 0;
    for (size_t i = 0; i < n; i += 3) {
        uint32_t v = (uint32_t)in[i] << 16;
        if (i + 1 < n) {
            v |= (uint32_t)in[i + 1] << 8;
        }
        if (i + 2 < n) {
            v |= in[i + 2];
        }
        out[o++] = tbl[v >> 18 & 63];
        out[o++] = tbl[v >> 12 & 63];
        out[o++] = i + 1 < n ? tbl[v >> 6 & 63] : '=';
        out[o++] = i + 2 < n ? tbl[v & 63] : '=';
    }
    out[o] = '\0';
}

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
    /* RFC 6455 §5.3: a fresh unpredictable mask per frame, so an intermediary
     * cannot be steered by attacker-chosen payload bytes. */
    unsigned char mask[4];
    random_bytes(mask, sizeof mask);
    memcpy(header + h, mask, 4);
    h += 4;
    if (send(fd, header, h, MSG_NOSIGNAL) != (ssize_t)h) {
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
    int rc = send(fd, masked, n, MSG_NOSIGNAL) == (ssize_t)n ? 0 : -1;
    free(masked);
    return rc;
}

/*
 * Next complete text message (malloc'd, NUL-terminated). Returns NULL on a
 * close frame or a dropped connection (*failed stays 0) and on a protocol
 * violation (*failed set, err filled): a frame or reassembled message larger
 * than DRSG_WS_MAX_MESSAGE_BYTES is refused before anything is allocated for
 * it, since the length field is the peer's word and not a promise we can
 * afford to keep.
 */
static char *ws_read_message(struct ws_rd *rd, int *failed, drsg_error *err) {
    unsigned char *msg = NULL;
    size_t msg_len = 0;
    *failed = 0;
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
        if (len > DRSG_WS_MAX_MESSAGE_BYTES || msg_len + len > DRSG_WS_MAX_MESSAGE_BYTES) {
            free(msg);
            *failed = 1;
            set_err(err, DRSG_TRANSPORT_ERROR_CODE,
                    "protocol error: websocket message exceeds DRSG_WS_MAX_MESSAGE_BYTES");
            return NULL;
        }
        unsigned char mask[4];
        if (masked && ws_read_exact(rd, mask, 4)) {
            free(msg);
            return NULL;
        }
        unsigned char *data = NULL;
        if (len) {
            data = malloc((size_t)len);
            if (!data || ws_read_exact(rd, data, (size_t)len)) {
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
            ws_send_frame(rd->fd, 0xA, data, (size_t)len);
            free(data);
            continue;
        }
        if (opcode == 0xA) { /* pong */
            free(data);
            continue;
        }
        /* text (0x1) or continuation (0x0): accumulate until FIN */
        unsigned char *grown = realloc(msg, msg_len + (size_t)len + 1);
        if (!grown) {
            free(data);
            free(msg);
            return NULL;
        }
        msg = grown;
        if (len) {
            memcpy(msg + msg_len, data, (size_t)len);
        }
        msg_len += (size_t)len;
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

/*
 * Locate an HTTP header by name (case-insensitive, RFC 7230) in the response
 * head and copy its trimmed value into out. Returns 0 when absent.
 */
static int ws_header_value(const unsigned char *head, size_t n, const char *name,
                           char *out, size_t out_len) {
    size_t m = strlen(name);
    for (size_t i = 0; i + m + 1 <= n; i++) {
        if ((i == 0 || head[i - 1] == '\n') && strncasecmp((const char *)head + i, name, m) == 0
            && head[i + m] == ':') {
            size_t v = i + m + 1;
            while (v < n && (head[v] == ' ' || head[v] == '\t')) {
                v++;
            }
            size_t e = v;
            while (e < n && head[e] != '\r' && head[e] != '\n') {
                e++;
            }
            while (e > v && (head[e - 1] == ' ' || head[e - 1] == '\t')) {
                e--;
            }
            if (e - v >= out_len) {
                return 0;
            }
            memcpy(out, head + v, e - v);
            out[e - v] = '\0';
            return 1;
        }
    }
    return 0;
}

/* Open a ws:// connection to <base_url>/ws and complete the handshake. */
static int ws_connect(CURL *curl, const char *base_url, const char *token, struct ws_rd *rd,
                      drsg_error *err) {
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

    /* The token rides the query string (browsers cannot set headers on a
     * WebSocket, so the server accepts it there); percent-encode it so a token
     * containing '&', '#' or '%' cannot rewrite the request line. */
    char *escaped = NULL;
    if (token && token[0]) {
        escaped = curl_easy_escape(curl, token, 0);
        if (!escaped) {
            close(fd);
            return set_err(err, -32000, "out of memory");
        }
    }

    /* A fresh 16-byte nonce per handshake (RFC 6455 §4.1); the server must
     * answer with base64(sha1(nonce + GUID)) and we check that it did, which is
     * what tells a WebSocket endpoint apart from any HTTP server that happens
     * to say 101. */
    unsigned char nonce[16];
    random_bytes(nonce, sizeof nonce);
    char key[25];
    base64_encode(nonce, sizeof nonce, key);
    char expect_src[24 + 36 + 1];
    snprintf(expect_src, sizeof expect_src, "%s258EAFA5-E914-47DA-95CA-C5AB0DC85B11", key);
    unsigned char digest[20];
    sha1((const unsigned char *)expect_src, strlen(expect_src), digest);
    char expect[29];
    base64_encode(digest, sizeof digest, expect);

    size_t req_cap = 512 + (escaped ? strlen(escaped) : 0) + strlen(host) + strlen(port);
    char *req = malloc(req_cap);
    if (!req) {
        curl_free(escaped);
        close(fd);
        return set_err(err, -32000, "out of memory");
    }
    int rn = snprintf(req, req_cap,
                      "GET /ws%s%s HTTP/1.1\r\n"
                      "Host: %s:%s\r\n"
                      "Upgrade: websocket\r\n"
                      "Connection: Upgrade\r\n"
                      "Sec-WebSocket-Key: %s\r\n"
                      "Sec-WebSocket-Version: 13\r\n\r\n",
                      escaped ? "?token=" : "", escaped ? escaped : "", host, port, key);
    curl_free(escaped);
    int sent = rn < 0 || rn >= (int)req_cap ? -1 : (int)send(fd, req, (size_t)rn, MSG_NOSIGNAL);
    free(req);
    if (sent != rn) {
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
    char accept[64];
    if (!ws_header_value(buf, (size_t)sep, "Sec-WebSocket-Accept", accept, sizeof accept)
        || strcmp(accept, expect) != 0) {
        close(fd);
        return set_err(err, DRSG_TRANSPORT_ERROR_CODE,
                       "protocol error: Sec-WebSocket-Accept does not match the key");
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

/*
 * The handle records the socket the watch is blocked on so that cancel can
 * shutdown() it from another thread, which fails the pending recv and lets
 * the watch loop unwind normally. The mutex orders "watch publishes its fd"
 * against "cancel reads it"; the flag covers a cancel that arrives before the
 * socket exists, so the watch returns right after connecting instead of
 * blocking forever on a subscription nobody wants.
 */
struct drsg_watch_ctl {
    pthread_mutex_t mu;
    int cancelled;
    int fd;
};

drsg_watch_ctl *drsg_watch_ctl_new(void) {
    drsg_watch_ctl *ctl = calloc(1, sizeof *ctl);
    if (!ctl) {
        return NULL;
    }
    if (pthread_mutex_init(&ctl->mu, NULL) != 0) {
        free(ctl);
        return NULL;
    }
    ctl->fd = -1;
    return ctl;
}

void drsg_watch_ctl_cancel(drsg_watch_ctl *ctl) {
    if (!ctl) {
        return;
    }
    pthread_mutex_lock(&ctl->mu);
    ctl->cancelled = 1;
    if (ctl->fd >= 0) {
        shutdown(ctl->fd, SHUT_RDWR);
    }
    pthread_mutex_unlock(&ctl->mu);
}

void drsg_watch_ctl_free(drsg_watch_ctl *ctl) {
    if (!ctl) {
        return;
    }
    pthread_mutex_destroy(&ctl->mu);
    free(ctl);
}

/* Publish (fd >= 0) or withdraw (fd < 0) the watched socket; 1 if cancelled. */
static int ctl_set_fd(drsg_watch_ctl *ctl, int fd) {
    if (!ctl) {
        return 0;
    }
    pthread_mutex_lock(&ctl->mu);
    ctl->fd = fd;
    int cancelled = ctl->cancelled;
    pthread_mutex_unlock(&ctl->mu);
    return cancelled;
}

int drsg_watch(drsg_client *c, const char *plane, const char *label,
               drsg_change_cb cb, void *userdata, drsg_error *err) {
    return drsg_watch_cancellable(c, plane, label, cb, userdata, NULL, err);
}

int drsg_watch_cancellable(drsg_client *c, const char *plane, const char *label,
                           drsg_change_cb cb, void *userdata, drsg_watch_ctl *ctl,
                           drsg_error *err) {
    if (err) {
        err->code = 0;
        err->message[0] = '\0';
    }
    if (!c || !plane || !cb) {
        return set_err(err, -32000, "drsg_watch: client, plane, and cb are required");
    }

    struct ws_rd rd = {.fd = -1};
    if (ws_connect(c->curl, c->base_url, c->token, &rd, err)) {
        return -1;
    }
    if (ctl_set_fd(ctl, rd.fd)) {
        ws_close(&rd);
        return 0; /* cancelled before we got here */
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
        ctl_set_fd(ctl, -1);
        ws_close(&rd);
        return set_err(err, -32000, "websocket subscribe failed");
    }

    int failed = 0;
    for (;;) {
        char *text = ws_read_message(&rd, &failed, err);
        if (!text) {
            break; /* clean close, cancellation, or a protocol failure */
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

    ctl_set_fd(ctl, -1);
    ws_close(&rd);
    return failed ? -1 : 0;
}
