// A minimal RFC 6455 server for tests: completes the upgrade, then hands the
// frame stream to a script. It exists to play the peer behaviours a healthy
// `drsg serve` never shows — staying silent through the handshake, ignoring a
// close frame, or pushing frames the listener chokes on.
package io.github.wangyingsm.drsg;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.Base64;
import java.util.concurrent.CompletableFuture;

final class FakeWebSocketServer implements AutoCloseable {

    /** What the server does once the socket is upgraded. */
    interface Script {
        void run(Conn conn) throws Exception;
    }

    /** One upgraded connection. */
    static final class Conn {
        final Socket socket;
        final InputStream in;
        final OutputStream out;

        Conn(Socket socket) throws IOException {
            this.socket = socket;
            this.in = socket.getInputStream();
            this.out = socket.getOutputStream();
        }

        /** Sends an unmasked text frame, as servers do. */
        void sendText(String text) throws IOException {
            byte[] payload = text.getBytes(StandardCharsets.UTF_8);
            byte[] header;
            if (payload.length < 126) {
                header = new byte[] {(byte) 0x81, (byte) payload.length};
            } else {
                header = new byte[] {(byte) 0x81, 126, (byte) (payload.length >> 8), (byte) payload.length};
            }
            out.write(header);
            out.write(payload);
            out.flush();
        }

        /** Reads one client frame and returns its opcode, or -1 at EOF. */
        int readFrameOpcode() throws IOException {
            int b0 = in.read();
            if (b0 < 0) {
                return -1;
            }
            int b1 = in.read();
            long len = b1 & 0x7F;
            if (len == 126) {
                len = ((long) in.read() << 8) | in.read();
            } else if (len == 127) {
                len = 0;
                for (int i = 0; i < 8; i++) {
                    len = (len << 8) | in.read();
                }
            }
            long skip = ((b1 & 0x80) != 0 ? 4 : 0) + len;
            while (skip > 0) {
                long n = in.skip(skip);
                if (n <= 0) {
                    if (in.read() < 0) {
                        return -1;
                    }
                    n = 1;
                }
                skip -= n;
            }
            return b0 & 0x0F;
        }
    }

    private final ServerSocket listener;
    private final Thread acceptor;
    /** Completes when the script has finished (or thrown). */
    final CompletableFuture<Void> scriptDone = new CompletableFuture<>();

    /**
     * @param upgrade whether to answer the HTTP upgrade at all; a server that
     *     accepts TCP and then says nothing models a stalled handshake
     */
    FakeWebSocketServer(boolean upgrade, Script script) throws IOException {
        listener = new ServerSocket(0, 1, java.net.InetAddress.getLoopbackAddress());
        acceptor = new Thread(() -> {
            try (Socket socket = listener.accept()) {
                socket.setSoTimeout(10_000);
                if (!upgrade) {
                    Thread.sleep(10_000); // outlive any client timeout under test
                    return;
                }
                handshake(socket);
                script.run(new Conn(socket));
                scriptDone.complete(null);
            } catch (Throwable t) {
                scriptDone.completeExceptionally(t);
            }
        }, "fake-ws");
        acceptor.setDaemon(true);
        acceptor.start();
    }

    String baseUrl() {
        return "http://127.0.0.1:" + listener.getLocalPort();
    }

    private static void handshake(Socket socket) throws Exception {
        BufferedReader r = new BufferedReader(new InputStreamReader(socket.getInputStream(), StandardCharsets.ISO_8859_1));
        String key = null;
        for (String line = r.readLine(); line != null && !line.isEmpty(); line = r.readLine()) {
            if (line.regionMatches(true, 0, "Sec-WebSocket-Key:", 0, 18)) {
                key = line.substring(18).trim();
            }
        }
        if (key == null) {
            throw new IllegalStateException("no Sec-WebSocket-Key in the upgrade request");
        }
        byte[] digest = MessageDigest.getInstance("SHA-1")
                .digest((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").getBytes(StandardCharsets.ISO_8859_1));
        String reply = "HTTP/1.1 101 Switching Protocols\r\n"
                + "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                + "Sec-WebSocket-Accept: " + Base64.getEncoder().encodeToString(digest) + "\r\n\r\n";
        socket.getOutputStream().write(reply.getBytes(StandardCharsets.ISO_8859_1));
        socket.getOutputStream().flush();
    }

    @Override
    public void close() throws IOException {
        listener.close();
        acceptor.interrupt();
    }
}
