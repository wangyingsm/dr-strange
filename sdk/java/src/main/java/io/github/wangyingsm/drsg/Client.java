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
import java.time.Duration;
import java.util.concurrent.atomic.AtomicLong;
import com.fasterxml.jackson.annotation.JsonInclude;

/** One endpoint's worth of config plus the JSON-RPC call primitive. */
public class Client {

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

    protected final String baseUrl;
    protected final String token;
    protected final Duration timeout;
    private final HttpClient http;
    private final AtomicLong id = new AtomicLong();

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
        this.baseUrl = stripTrailingSlash(baseUrl == null ? DEFAULT_BASE_URL : baseUrl);
        this.token = token;
        this.timeout = Duration.ofSeconds(30);
        this.http = HttpClient.newHttpClient();
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
}
