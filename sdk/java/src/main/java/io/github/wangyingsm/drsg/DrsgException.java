package io.github.wangyingsm.drsg;

import com.fasterxml.jackson.databind.JsonNode;

/** A JSON-RPC error returned by the server (carries the numeric {@link #code()}). */
public class DrsgException extends Exception {

    private final int code;
    private final transient JsonNode data;

    public DrsgException(int code, String message, JsonNode data) {
        super(message + " (code " + code + ")");
        this.code = code;
        this.data = data;
    }

    /** The JSON-RPC error code. */
    public int code() {
        return code;
    }

    /** The optional structured {@code data} payload, or {@code null}. */
    public JsonNode data() {
        return data;
    }
}
