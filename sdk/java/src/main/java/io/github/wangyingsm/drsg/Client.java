// Base JSON-RPC 2.0 transport for dr-strange (`drsg serve`).
//
// The typed method surface lives in the generated Drsg.java (see the codegen
// source set and the `generate` Gradle task); this class is the hand-written
// core it extends. JSON is handled by Jackson; HTTP by the JDK HttpClient.
package io.github.wangyingsm.drsg;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.PropertyNamingStrategies;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.io.IOException;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.net.http.WebSocket;
import java.time.Duration;
import java.util.Set;
import java.util.concurrent.CompletionException;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import com.fasterxml.jackson.annotation.JsonInclude;

/**
 * One endpoint's worth of config plus the JSON-RPC call primitive.
 *
 * <p>A client owns background resources — the JDK {@link HttpClient}'s
 * executor and any open {@link Subscription} — so it is {@link AutoCloseable}:
 * {@link #close()} ends every subscription and, when the client built its own
 * {@code HttpClient}, shuts that client's executor down. An {@code HttpClient}
 * injected through {@link Options#httpClient} is shared and left alone.
 */
public class Client implements AutoCloseable {

    /** Timeout applied when {@link Options#timeout} is not set. */
    public static final Duration DEFAULT_TIMEOUT = Duration.ofSeconds(30);

    /**
     * Construction options. Every field is optional; unset ones take the same
     * defaults as the positional constructors.
     */
    public static final class Options {
        private String baseUrl;
        private String token = System.getenv("DRSG_TOKEN");
        private Duration timeout = DEFAULT_TIMEOUT;
        private HttpClient httpClient;

        /** Endpoint; default {@link #DEFAULT_BASE_URL}. */
        public Options baseUrl(String baseUrl) {
            this.baseUrl = baseUrl;
            return this;
        }

        /** Bearer token; default {@code $DRSG_TOKEN}. */
        public Options token(String token) {
            this.token = token;
            return this;
        }

        /**
         * Bound on each RPC round trip and on a {@link #watch} handshake;
         * default {@link #DEFAULT_TIMEOUT}. Must be positive.
         */
        public Options timeout(Duration timeout) {
            if (timeout == null || timeout.isZero() || timeout.isNegative()) {
                throw new IllegalArgumentException("timeout must be positive");
            }
            this.timeout = timeout;
            return this;
        }

        /**
         * An {@link HttpClient} to share (connection pool, proxy, TLS context,
         * executor). The client does not own it: {@link Client#close()} leaves
         * it running. Default: one private client per {@code Client}.
         */
        public Options httpClient(HttpClient httpClient) {
            this.httpClient = httpClient;
            return this;
        }
    }

    /** Endpoint used when none is configured. */
    public static final String DEFAULT_BASE_URL = "http://127.0.0.1:7700";

    /** JSON-RPC error code for a missing/invalid credential. */
    public static final int AUTH_ERROR_CODE = -32001;

    /**
     * Shared mapper. Wire field names are snake_case; Java uses camelCase, so a
     * snake-case naming strategy bridges them. Nulls are dropped on the wire
     * (absent optional params), and unknown result fields are ignored.
     */
    protected static final ObjectMapper MAPPER = new ObjectMapper()
            .setPropertyNamingStrategy(PropertyNamingStrategies.SNAKE_CASE)
            .setSerializationInclusion(JsonInclude.Include.NON_NULL)
            .configure(com.fasterxml.jackson.databind.DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, false);

    private static final System.Logger LOG = System.getLogger(Client.class.getName());

    protected final String baseUrl;
    protected final String token;
    protected final Duration timeout;
    private final HttpClient http;
    /** Non-null only when this client built {@link #http} and must reap it. */
    private final ExecutorService ownedExecutor;
    private final AtomicLong id = new AtomicLong();
    private final Set<WebSocketSubscription> subscriptions = ConcurrentHashMap.newKeySet();
    private final AtomicBoolean closed = new AtomicBoolean();

    /** Default endpoint; token from {@code $DRSG_TOKEN}. */
    public Client() {
        this(DEFAULT_BASE_URL, System.getenv("DRSG_TOKEN"));
    }

    /** Custom endpoint; token from {@code $DRSG_TOKEN}. */
    public Client(String baseUrl) {
        this(baseUrl, System.getenv("DRSG_TOKEN"));
    }

    /** Custom endpoint and token (either may be null to accept the default). */
    public Client(String baseUrl, String token) {
        this(new Options().baseUrl(baseUrl).token(token));
    }

    /** Full configuration: see {@link Options}. */
    public Client(Options options) {
        this.baseUrl = stripTrailingSlash(options.baseUrl == null ? DEFAULT_BASE_URL : options.baseUrl);
        this.token = options.token;
        this.timeout = options.timeout;
        if (options.httpClient != null) {
            this.http = options.httpClient;
            this.ownedExecutor = null;
        } else {
            // The JDK's default executor is a cached pool of non-daemon
            // threads that only exits once the HttpClient is garbage
            // collected. Supplying our own pool (daemon threads, so a
            // forgotten client cannot pin the JVM) gives close() something to
            // shut down deterministically.
            this.ownedExecutor = Executors.newCachedThreadPool(r -> {
                Thread t = new Thread(r, "drsg-http");
                t.setDaemon(true);
                return t;
            });
            this.http = HttpClient.newBuilder()
                    .connectTimeout(options.timeout)
                    .executor(ownedExecutor)
                    .build();
        }
    }

    /** The per-call timeout this client applies (see {@link Options#timeout}). */
    public Duration timeout() {
        return timeout;
    }

    /**
     * Ends every open {@link Subscription} and, if this client built its own
     * {@link HttpClient}, shuts down that client's threads. Idempotent; a
     * later {@link #call} or {@link #watch} fails with a {@link DrsgException}.
     */
    @Override
    public void close() {
        if (!closed.compareAndSet(false, true)) {
            return;
        }
        for (WebSocketSubscription sub : subscriptions) {
            sub.close();
        }
        if (ownedExecutor != null) {
            ownedExecutor.shutdownNow();
        }
    }

    private void ensureOpen() throws DrsgException {
        if (closed.get()) {
            throw new DrsgException(-32000, "client is closed", null);
        }
    }

    private static String stripTrailingSlash(String s) {
        int end = s.length();
        while (end > 0 && s.charAt(end - 1) == '/') {
            end--;
        }
        return s.substring(0, end);
    }

    /** Send one JSON-RPC request and deserialize its result into {@code type}. */
    protected <T> T call(String method, Object params, TypeReference<T> type) throws DrsgException {
        ensureOpen();
        ObjectNode req = MAPPER.createObjectNode();
        req.put("jsonrpc", "2.0");
        req.put("method", method);
        req.put("id", id.incrementAndGet());
        if (params != null) {
            req.set("params", MAPPER.valueToTree(params));
        }

        String body;
        try {
            body = MAPPER.writeValueAsString(req);
        } catch (JsonProcessingException e) {
            throw new DrsgException(-32000, "encode request: " + e.getMessage(), null);
        }

        HttpRequest.Builder rb = HttpRequest.newBuilder(URI.create(baseUrl + "/rpc"))
                .header("content-type", "application/json")
                .timeout(timeout)
                .POST(HttpRequest.BodyPublishers.ofString(body));
        if (token != null && !token.isEmpty()) {
            rb.header("authorization", "Bearer " + token);
        }

        HttpResponse<String> resp;
        try {
            resp = http.send(rb.build(), HttpResponse.BodyHandlers.ofString());
        } catch (java.net.http.HttpTimeoutException e) {
            throw new DrsgException(-32000, "timeout after " + timeout + ": " + e.getMessage(), null);
        } catch (IOException e) {
            throw new DrsgException(-32000, "connection failed: " + e.getMessage(), null);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new DrsgException(-32000, "interrupted: " + e.getMessage(), null);
        }

        if (resp.statusCode() / 100 != 2) {
            // A transport-level refusal (403 cross-origin, 413 too large) arrives
            // as HTTP, not a JSON-RPC error — surface it uniformly.
            throw new DrsgException(-32000, "HTTP " + resp.statusCode(), null);
        }

        JsonNode msg;
        try {
            msg = MAPPER.readTree(resp.body());
        } catch (JsonProcessingException e) {
            throw new DrsgException(-32000, "decode response: " + e.getMessage(), null);
        }

        JsonNode err = msg.get("error");
        if (err != null && !err.isNull()) {
            int code = err.path("code").asInt(-32000);
            String m = err.path("message").asText("error");
            JsonNode data = err.get("data");
            if (code == AUTH_ERROR_CODE) {
                throw new DrsgAuthException(code, m, data);
            }
            throw new DrsgException(code, m, data);
        }

        JsonNode result = msg.get("result");
        if (result == null || result.isNull()) {
            return null;
        }
        try {
            return MAPPER.convertValue(result, type);
        } catch (IllegalArgumentException e) {
            throw new DrsgException(-32000, "decode result: " + e.getMessage(), null);
        }
    }

    /** Receives change events from a {@link #watch} subscription. */
    @FunctionalInterface
    public interface ChangeListener {
        void onChange(ChangeEvent event);
    }

    /** A live change-feed subscription; {@link #close()} stops it. */
    public interface Subscription extends AutoCloseable {
        @Override
        void close();
    }

    /**
     * Subscribe to a plane's change feed (ROADMAP §5) over a long-lived
     * WebSocket. {@code listener} is invoked with each committed
     * {@link ChangeEvent} until the returned {@link Subscription} is closed.
     * Pass a {@code label} (or null) to receive only node changes carrying it.
     *
     * <p>Uses the JDK's built-in {@link WebSocket}. Best-effort — a slow
     * listener can miss commits, and reconnecting after a drop is the caller's
     * to add. The connection is established before this returns, within the
     * client's timeout. An exception thrown by {@code listener} is logged (at
     * {@code WARNING}, logger {@code io.github.wangyingsm.drsg.Client}) and the
     * subscription continues; the listener runs on the HttpClient's executor,
     * so it must not block for long.
     */
    public Subscription watch(String plane, String label, ChangeListener listener) throws DrsgException {
        ensureOpen();
        // The token is a bearer header on the upgrade, the form the server
        // prefers (arch/08-web-ui §4.1). `?token=` exists only for browsers,
        // whose WebSocket API cannot set headers; a URL credential would end
        // up in proxy and access logs, so this client never sends one.
        String url = baseUrl.replaceFirst("^http", "ws") + "/ws";

        WebSocket.Listener wl = new WebSocket.Listener() {
            private final StringBuilder buf = new StringBuilder();

            @Override
            public void onOpen(WebSocket ws) {
                ObjectNode sub = MAPPER.createObjectNode();
                sub.put("plane", plane);
                if (label != null && !label.isEmpty()) {
                    sub.put("label", label);
                }
                ObjectNode req = MAPPER.createObjectNode();
                req.put("jsonrpc", "2.0");
                req.put("method", "plane.watch");
                req.set("params", sub);
                ws.sendText(req.toString(), true);
                ws.request(1);
            }

            @Override
            public CompletionStage<?> onText(WebSocket ws, CharSequence data, boolean last) {
                buf.append(data);
                if (last) {
                    String msg = buf.toString();
                    buf.setLength(0);
                    dispatch(msg);
                }
                ws.request(1);
                return null;
            }

            private void dispatch(String msg) {
                ChangeEvent event;
                try {
                    JsonNode node = MAPPER.readTree(msg);
                    if (!"plane.change".equals(node.path("method").asText())) {
                        return; // the subscribe ack, or something newer than this SDK
                    }
                    event = MAPPER.convertValue(node.get("params"), ChangeEvent.class);
                } catch (IllegalArgumentException | JsonProcessingException e) {
                    // A malformed frame shouldn't tear down the subscription,
                    // but it shouldn't vanish either.
                    LOG.log(System.Logger.Level.DEBUG, "drsg change feed: undecodable frame ignored", e);
                    return;
                }
                if (event == null) {
                    // convertValue(null) is null, not an exception: a
                    // plane.change with absent or null params has nothing to
                    // deliver, and must not reach a listener as null.
                    LOG.log(System.Logger.Level.DEBUG, "drsg change feed: plane.change without params ignored");
                    return;
                }
                long seq = event.seq();
                try {
                    listener.onChange(event);
                } catch (RuntimeException e) {
                    // The listener runs on the HttpClient's thread; letting
                    // the throw escape would kill the WebSocket silently.
                    LOG.log(System.Logger.Level.WARNING,
                            "drsg change feed: listener threw on seq " + seq + "; subscription continues", e);
                }
            }

            @Override
            public void onError(WebSocket ws, Throwable error) {
                LOG.log(System.Logger.Level.WARNING, "drsg change feed: connection failed", error);
            }
        };

        final WebSocket ws;
        try {
            // connectTimeout bounds the TCP/TLS connect; orTimeout bounds the
            // whole future so a peer that accepts and then stays silent through
            // the upgrade cannot pin the caller in join().
            WebSocket.Builder wb = http.newWebSocketBuilder().connectTimeout(timeout);
            if (token != null && !token.isEmpty()) {
                wb.header("authorization", "Bearer " + token);
            }
            ws = wb.buildAsync(URI.create(url), wl)
                    .orTimeout(timeout.toMillis(), TimeUnit.MILLISECONDS)
                    .join();
        } catch (CompletionException e) {
            Throwable cause = e.getCause() != null ? e.getCause() : e;
            String why = cause instanceof TimeoutException
                    ? "no handshake within " + timeout
                    : cause.getMessage();
            throw new DrsgException(-32000, "websocket connect failed: " + why, null);
        }
        WebSocketSubscription sub = new WebSocketSubscription(ws);
        subscriptions.add(sub);
        if (closed.get()) {
            // close() raced with the handshake; do not leave a live socket.
            sub.close();
            throw new DrsgException(-32000, "client is closed", null);
        }
        return sub;
    }

    /** How long {@link Subscription#close()} waits for its close frame to be written. */
    private static final Duration CLOSE_GRACE = Duration.ofSeconds(2);

    private final class WebSocketSubscription implements Subscription {
        private final WebSocket ws;
        private final AtomicBoolean done = new AtomicBoolean();

        WebSocketSubscription(WebSocket ws) {
            this.ws = ws;
        }

        /**
         * Sends a close frame (waiting briefly for it to be written), then
         * aborts the socket regardless. sendClose alone leaves the JDK
         * WebSocket waiting for the peer's close reply, so a peer that never
         * answers the close handshake would keep the connection (and its
         * receive loop) alive indefinitely; abort() settles it.
         */
        @Override
        public void close() {
            if (!done.compareAndSet(false, true)) {
                return;
            }
            subscriptions.remove(this);
            try {
                ws.sendClose(WebSocket.NORMAL_CLOSURE, "").get(CLOSE_GRACE.toMillis(), TimeUnit.MILLISECONDS);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            } catch (ExecutionException | TimeoutException | RuntimeException e) {
                // Already gone, or not answering — abort() below settles it.
            } finally {
                ws.abort();
            }
        }
    }
}
