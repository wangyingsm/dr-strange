// Transport behaviours that need a misbehaving peer, played by
// FakeWebSocketServer rather than a real `drsg serve`.
package io.github.wangyingsm.drsg;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.net.http.HttpClient;
import java.time.Duration;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.Logger;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;

class ClientTransportTest {

    private static final String CHANGE = "{\"jsonrpc\":\"2.0\",\"method\":\"plane.change\",\"params\":"
            + "{\"plane\":\"p\",\"seq\":%d,\"truncated\":false,\"changes\":[]}}";

    @Test
    @Timeout(10)
    void watchHandshakeIsBoundedByTheClientTimeout() throws Exception {
        try (FakeWebSocketServer srv = new FakeWebSocketServer(false, c -> { });
             Client client = new Client(new Client.Options().baseUrl(srv.baseUrl()).timeout(Duration.ofMillis(300)))) {
            long start = System.nanoTime();
            DrsgException ex = assertThrows(DrsgException.class, () -> client.watch("p", null, ev -> { }));
            long elapsedMs = (System.nanoTime() - start) / 1_000_000;
            assertTrue(elapsedMs < 5_000, "watch() hung for " + elapsedMs + " ms on a silent peer");
            assertEquals(-32000, ex.code());
            assertTrue(ex.getMessage().contains("PT0.3S"), ex.getMessage());
        }
    }

    @Test
    @Timeout(10)
    void subscriptionCloseAbortsAPeerThatIgnoresTheCloseHandshake() throws Exception {
        CountDownLatch peerSawEof = new CountDownLatch(1);
        try (FakeWebSocketServer srv = new FakeWebSocketServer(true, c -> {
                    // Swallow every frame, including the close frame, and never
                    // reply: only an abort() on the client side ends this.
                    while (c.readFrameOpcode() >= 0) {
                        // keep reading
                    }
                    peerSawEof.countDown();
                });
             Client client = new Client(new Client.Options().baseUrl(srv.baseUrl()))) {
            Client.Subscription sub = client.watch("p", null, ev -> { });
            long start = System.nanoTime();
            sub.close();
            long elapsedMs = (System.nanoTime() - start) / 1_000_000;
            assertTrue(elapsedMs < 5_000, "close() took " + elapsedMs + " ms");
            assertTrue(peerSawEof.await(5, TimeUnit.SECONDS), "the socket was not torn down after close()");
            sub.close(); // idempotent
        }
    }

    @Test
    @Timeout(10)
    void listenerExceptionIsLoggedAndTheFeedContinues() throws Exception {
        List<LogRecord> logged = new CopyOnWriteArrayList<>();
        Handler capture = new Handler() {
            @Override
            public void publish(LogRecord record) {
                logged.add(record);
            }

            @Override
            public void flush() { }

            @Override
            public void close() { }
        };
        // System.Logger's default backend is java.util.logging, under the
        // same logger name.
        Logger jul = Logger.getLogger(Client.class.getName());
        jul.addHandler(capture);
        CountDownLatch secondEvent = new CountDownLatch(1);
        List<Long> seen = new CopyOnWriteArrayList<>();
        try (FakeWebSocketServer srv = new FakeWebSocketServer(true, c -> {
                    c.readFrameOpcode(); // the subscribe request
                    c.sendText("not json at all");
                    c.sendText(String.format(CHANGE, 1));
                    c.sendText(String.format(CHANGE, 2));
                    while (c.readFrameOpcode() >= 0) {
                        // until the client hangs up
                    }
                });
             Client client = new Client(new Client.Options().baseUrl(srv.baseUrl()))) {
            Client.Subscription sub = client.watch("p", null, ev -> {
                seen.add(ev.seq());
                if (ev.seq() == 1) {
                    throw new IllegalStateException("listener bug");
                }
                secondEvent.countDown();
            });
            assertTrue(secondEvent.await(5, TimeUnit.SECONDS), "feed stopped after the listener threw");
            assertEquals(List.of(1L, 2L), seen);
            sub.close();
        } finally {
            jul.removeHandler(capture);
        }
        LogRecord warning = logged.stream()
                .filter(r -> r.getLevel().intValue() >= Level.WARNING.intValue())
                .findFirst()
                .orElseThrow(() -> new AssertionError("listener exception was not logged: " + logged));
        assertTrue(warning.getThrown() instanceof IllegalStateException, String.valueOf(warning.getThrown()));
        assertTrue(warning.getMessage().contains("seq 1"), warning.getMessage());
    }

    @Test
    @Timeout(10)
    void closeEndsSubscriptionsAndRefusesFurtherUse() throws Exception {
        CountDownLatch peerSawEof = new CountDownLatch(1);
        try (FakeWebSocketServer srv = new FakeWebSocketServer(true, c -> {
                    while (c.readFrameOpcode() >= 0) {
                        // keep reading
                    }
                    peerSawEof.countDown();
                })) {
            Client client = new Client(new Client.Options().baseUrl(srv.baseUrl()));
            client.watch("p", null, ev -> { });
            client.close();
            assertTrue(peerSawEof.await(5, TimeUnit.SECONDS), "close() left the subscription open");
            DrsgException ex = assertThrows(DrsgException.class, () -> client.watch("p", null, ev -> { }));
            assertTrue(ex.getMessage().contains("closed"), ex.getMessage());
            client.close(); // idempotent
        }
    }

    @Test
    void injectedHttpClientIsSharedAndOutlivesClose() throws Exception {
        HttpClient shared = HttpClient.newHttpClient();
        Client a = new Client(new Client.Options().baseUrl("http://127.0.0.1:1").httpClient(shared));
        a.close();
        // The shared client still works for another Client after the first closed.
        try (FakeWebSocketServer srv = new FakeWebSocketServer(true, c -> {
                    while (c.readFrameOpcode() >= 0) {
                        // keep reading
                    }
                });
             Client b = new Client(new Client.Options().baseUrl(srv.baseUrl()).httpClient(shared))) {
            b.watch("p", null, ev -> { }).close();
        }
        assertFalse(shared.executor().isPresent() && ((java.util.concurrent.ExecutorService) shared.executor().get()).isShutdown());
    }

    @Test
    @Timeout(10)
    void typedClientAcceptsOptions() throws Exception {
        // The generated Drsg must expose the Options constructor: Client.call is
        // protected, so without it timeout/httpClient are unreachable from the
        // typed API.
        try (FakeWebSocketServer srv = new FakeWebSocketServer(false, c -> { });
             Drsg db = new Drsg(new Client.Options().baseUrl(srv.baseUrl()).timeout(Duration.ofMillis(300)))) {
            DrsgException ex = assertThrows(DrsgException.class, () -> db.watch("p", null, ev -> { }));
            assertTrue(ex.getMessage().contains("PT0.3S"), ex.getMessage());
        }
    }

    @Test
    void optionsRejectNonPositiveTimeout() {
        assertThrows(IllegalArgumentException.class, () -> new Client.Options().timeout(Duration.ZERO));
        assertThrows(IllegalArgumentException.class, () -> new Client.Options().timeout(null));
    }
}
