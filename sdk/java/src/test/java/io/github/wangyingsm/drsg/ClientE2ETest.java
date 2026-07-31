// End-to-end tests: drive a real `drsg serve` over the client.
//
// Requires the `drsg` binary. Point at it with $DRSG_BIN, else the workspace
// target/{debug,release}/drsg is used; the suite skips if none is found.
package io.github.wangyingsm.drsg;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import java.io.File;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

class ClientE2ETest {

    private static final String TOKEN = "test-token";
    private static Process server;
    private static String baseUrl;

    @BeforeAll
    static void startServer() throws Exception {
        String bin = findBinary();
        if (bin == null) {
            return; // tests assumeTrue(baseUrl != null) and skip
        }
        int port;
        try (ServerSocket s = new ServerSocket(0)) {
            port = s.getLocalPort();
        }
        String addr = "127.0.0.1:" + port;
        Path db = Files.createTempDirectory("drsg-java-").resolve("sdk-test.drsg");

        ProcessBuilder pb = new ProcessBuilder(
                bin, "--db", db.toString(), "serve", "--addr", addr);
        pb.environment().put("DRSG_TOKEN", TOKEN);
        pb.redirectOutput(ProcessBuilder.Redirect.DISCARD);
        pb.redirectError(ProcessBuilder.Redirect.DISCARD);
        server = pb.start();

        long deadline = System.currentTimeMillis() + 10_000;
        while (System.currentTimeMillis() < deadline) {
            try (Socket probe = new Socket()) {
                probe.connect(new InetSocketAddress("127.0.0.1", port), 100);
                baseUrl = "http://" + addr;
                return;
            } catch (Exception retry) {
                Thread.sleep(50);
            }
        }
        throw new IllegalStateException("server never started listening");
    }

    @AfterAll
    static void stopServer() {
        if (server != null) {
            server.destroy();
        }
    }

    private static String findBinary() {
        String env = System.getenv("DRSG_BIN");
        if (env != null && !env.isEmpty()) {
            return new File(env).exists() ? env : null;
        }
        Path root = Path.of(System.getProperty("user.dir")).resolve("../../").normalize();
        for (String profile : List.of("debug", "release")) {
            File cand = root.resolve("target/" + profile + "/drsg").toFile();
            if (cand.exists()) {
                return cand.getAbsolutePath();
            }
        }
        return null;
    }

    @Test
    void crudRoundtrip() throws Exception {
        assumeTrue(baseUrl != null, "drsg binary not found; run `cargo build -p dr-strange-cli`");
        Drsg db = new Drsg(baseUrl, TOKEN);

        assertEquals(0, db.dbStats().nodes());

        Drsg.NodeRecord alice = db.nodeCreate(
                Drsg.NodeCreateParams.of("startup").withKey("alice").withLabels(List.of("Person")));
        assertEquals("alice", alice.externalKey());
        db.nodeCreate(Drsg.NodeCreateParams.of("startup").withKey("bob").withLabels(List.of("Person")));

        Drsg.EdgeRecord edge = db.edgeCreate(
                Drsg.EdgeCreateParams.of("startup", "alice", "bob", "KNOWS"));
        assertEquals("KNOWS", edge.type());

        // Property patch: set then unset, with types preserved.
        Drsg.NodeRecord upd = db.nodeUpdate(
                Drsg.NodeUpdateParams.of("startup").withKey("alice").withSet(Map.of("age", 41, "city", "NYC")));
        assertEquals(41, ((Number) upd.properties().get("age")).intValue());
        upd = db.nodeUpdate(Drsg.NodeUpdateParams.of("startup").withKey("alice").withUnset(List.of("city")));
        assertFalse(upd.properties().containsKey("city"));

        Drsg.NodeRecord got = db.nodeGet(Drsg.NodeGetParams.of("startup").withKey("alice"));
        assertNotNull(got);
        assertEquals(41, ((Number) got.properties().get("age")).intValue());

        // Delete cascades the edge; the graph is left consistent.
        assertTrue(db.nodeDelete(Drsg.NodeDeleteParams.of("startup").withKey("alice")).deleted());
        Drsg.DbStats stats = db.dbStats();
        assertEquals(1, stats.nodes());
        assertEquals(0, stats.edges());
    }

    @Test
    void planeAdmin() throws Exception {
        assumeTrue(baseUrl != null, "drsg binary not found");
        Drsg db = new Drsg(baseUrl, TOKEN);
        assertEquals("notes", db.planeCreate(Drsg.PlaneCreateParams.of("notes")).name());
        assertEquals("archive", db.planeRename(Drsg.PlaneRenameParams.of("notes", "archive")).name());
        assertTrue(db.planeDelete(Drsg.PlaneDeleteParams.of("archive")).deleted());
    }

    @Test
    void discover() throws Exception {
        assumeTrue(baseUrl != null, "drsg binary not found");
        Drsg db = new Drsg(baseUrl, TOKEN);
        Map<String, Object> doc = db.rpcDiscover();
        assertEquals("1.2.6", doc.get("openrpc"));
    }

    @Test
    void changeFeedOverWebSocket() throws Exception {
        assumeTrue(baseUrl != null, "drsg binary not found");
        Drsg db = new Drsg(baseUrl, TOKEN);

        List<ChangeEvent> events = new CopyOnWriteArrayList<>();
        CountDownLatch latch = new CountDownLatch(1);
        Client.Subscription sub = db.watch("startup", "Widget", ev -> {
            events.add(ev);
            latch.countDown();
        });
        try {
            Thread.sleep(300); // let the server register the subscription
            db.nodeCreate(Drsg.NodeCreateParams.of("startup").withKey("ws-widget").withLabels(List.of("Widget")));

            assertTrue(latch.await(3, TimeUnit.SECONDS), "no change event received over the websocket");
            ChangeEvent ev = events.get(0);
            assertTrue(ev.seq() > 0);
            ChangeEvent.Change c = ev.changes().stream()
                    .filter(x -> x.record() != null && "ws-widget".equals(x.record().get("external_key")))
                    .findFirst()
                    .orElseThrow();
            assertEquals("node", c.kind());
            assertEquals("created", c.op());
            assertTrue(c.labels().contains("Widget"));

            // Leave the graph as we found it — crudRoundtrip asserts on the
            // global node count and the class shares one server.
            db.nodeDelete(Drsg.NodeDeleteParams.of("startup").withKey("ws-widget"));
        } finally {
            sub.close();
        }
    }

    @Test
    void badTokenRaisesAuthError() throws Exception {
        assumeTrue(baseUrl != null, "drsg binary not found");
        Drsg db = new Drsg(baseUrl, "wrong");
        DrsgAuthException ex = assertThrows(DrsgAuthException.class, db::dbStats);
        assertEquals(Client.AUTH_ERROR_CODE, ex.code());
    }
}
