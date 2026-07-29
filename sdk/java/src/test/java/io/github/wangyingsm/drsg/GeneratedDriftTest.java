package io.github.wangyingsm.drsg;

import static org.junit.jupiter.api.Assertions.assertEquals;

import io.github.wangyingsm.drsg.codegen.Codegen;
import java.nio.file.Files;
import java.nio.file.Path;
import org.junit.jupiter.api.Test;

/** The committed Drsg.java must match the schema (no manual drift). */
class GeneratedDriftTest {

    @Test
    void generatedIsCurrent() throws Exception {
        Path base = Path.of(System.getProperty("user.dir"));
        String schema = Files.readString(base.resolve("../../crates/dr-strange-web/openrpc.json"));
        String expected = Codegen.render(schema);
        String actual = Files.readString(base.resolve("src/main/java/io/github/wangyingsm/drsg/Drsg.java"));
        assertEquals(expected, actual, "Drsg.java is stale — run `mvn -q compile exec:java`");
    }
}
