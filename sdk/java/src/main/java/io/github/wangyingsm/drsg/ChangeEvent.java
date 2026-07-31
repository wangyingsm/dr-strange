// Change-feed payload (ROADMAP §5): all the changes one commit produced,
// delivered over the live WebSocket (see Client.watch).
package io.github.wangyingsm.drsg;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import java.util.List;
import java.util.Map;

/**
 * All the changes one commit produced, tagged with the plane and the commit
 * sequence they landed at (address a time-travel read with {@code seq}).
 */
@JsonIgnoreProperties(ignoreUnknown = true)
public record ChangeEvent(String plane, long seq, boolean truncated, List<Change> changes) {

    /** One node or edge that changed in a commit. */
    @JsonIgnoreProperties(ignoreUnknown = true)
    public record Change(
            /** {@code "node"} or {@code "edge"}. */
            String kind,
            /** {@code "created"}, {@code "updated"}, or {@code "deleted"}. */
            String op,
            long id,
            /** Node labels (null for edges, and for a deleted node). */
            List<String> labels,
            /**
             * The committed record for a create/update (embeddings and
             * {@code _}-prefixed props stripped); null for a delete — read
             * {@code as_of = seq - 1} for the before-state.
             */
            Map<String, Object> record) {}
}
