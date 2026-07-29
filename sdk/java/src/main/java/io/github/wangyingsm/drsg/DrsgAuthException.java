package io.github.wangyingsm.drsg;

import com.fasterxml.jackson.databind.JsonNode;

/**
 * The server rejected the credential (code {@code -32001}).
 *
 * <p>Set a valid token via a {@code Drsg(baseUrl, token)} constructor or the
 * {@code DRSG_TOKEN} environment variable. With no token configured server-side,
 * only the same-origin browser UI is authorized — a programmatic client must
 * present one.
 */
public class DrsgAuthException extends DrsgException {

    public DrsgAuthException(int code, String message, JsonNode data) {
        super(code, message, data);
    }
}
