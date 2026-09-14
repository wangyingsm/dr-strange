// Unit tests for the hand-rolled WebSocket client behind Watch. These run
// against an in-process fake server rather than `drsg serve`, because they
// exercise the peer behaviours a well-behaved server never shows: closing
// first, never answering the upgrade, and announcing an absurd frame length.
package drsg

import (
	"bufio"
	"context"
	"crypto/sha1"
	"encoding/base64"
	"encoding/binary"
	"errors"
	"net"
	"net/http"
	"net/http/httptest"
	"runtime"
	"strings"
	"testing"
	"time"
)

// fakeWS serves /ws by completing the RFC 6455 upgrade and handing the raw
// socket to handle; the socket is closed when handle returns.
func fakeWS(t *testing.T, handle func(conn net.Conn, r *bufio.Reader)) *httptest.Server {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		key := req.Header.Get("Sec-WebSocket-Key")
		sum := sha1.Sum([]byte(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))
		hj, ok := w.(http.Hijacker)
		if !ok {
			t.Error("response writer cannot hijack")
			return
		}
		conn, rw, err := hj.Hijack()
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.Close()
		_, _ = rw.WriteString("HTTP/1.1 101 Switching Protocols\r\n" +
			"Upgrade: websocket\r\nConnection: Upgrade\r\n" +
			"Sec-WebSocket-Accept: " + base64.StdEncoding.EncodeToString(sum[:]) + "\r\n\r\n")
		_ = rw.Flush()
		handle(conn, rw.Reader)
	}))
	t.Cleanup(srv.Close)
	return srv
}

// readClientFrame consumes one masked client frame (the subscribe request or
// the close frame) so the fake can sequence itself against the client.
func readClientFrame(r *bufio.Reader) (opcode byte, err error) {
	var h [2]byte
	if _, err := readFullFrom(r, h[:]); err != nil {
		return 0, err
	}
	length := int(h[1] & 0x7F)
	switch length {
	case 126:
		var b [2]byte
		if _, err := readFullFrom(r, b[:]); err != nil {
			return 0, err
		}
		length = int(binary.BigEndian.Uint16(b[:]))
	case 127:
		var b [8]byte
		if _, err := readFullFrom(r, b[:]); err != nil {
			return 0, err
		}
		length = int(binary.BigEndian.Uint64(b[:]))
	}
	skip := make([]byte, 4+length) // mask key + payload
	if _, err := readFullFrom(r, skip); err != nil {
		return 0, err
	}
	return h[0] & 0x0F, nil
}

func readFullFrom(r *bufio.Reader, buf []byte) (int, error) {
	n := 0
	for n < len(buf) {
		m, err := r.Read(buf[n:])
		n += m
		if err != nil {
			return n, err
		}
	}
	return n, nil
}

// waitGoroutines polls until the goroutine count is back at or below base, or
// fails the test: the leak this guards against parks a goroutine forever, so
// a bounded wait is enough to tell the two apart.
func waitGoroutines(t *testing.T, base int) {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if runtime.NumGoroutine() <= base {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	buf := make([]byte, 1<<16)
	n := runtime.Stack(buf, true)
	t.Fatalf("goroutines did not return to %d (now %d):\n%s", base, runtime.NumGoroutine(), buf[:n])
}

func TestWatchServerClosesFirstLeavesNoGoroutine(t *testing.T) {
	srv := fakeWS(t, func(conn net.Conn, r *bufio.Reader) {
		_, _ = readClientFrame(r) // the subscribe request
		// Server-initiated close: frame then socket, as a restarting server does.
		_, _ = conn.Write([]byte{0x88, 0x00})
	})
	base := runtime.NumGoroutine()

	// A background ctx is the leaking case: nothing ever cancels it, so a
	// goroutine parked on ctx.Done() would live for the rest of the process.
	events, err := New(WithBaseURL(srv.URL)).Watch(context.Background(), "p")
	if err != nil {
		t.Fatal(err)
	}
	select {
	case _, ok := <-events:
		if ok {
			t.Fatal("unexpected event before the server-side close")
		}
	case <-time.After(3 * time.Second):
		t.Fatal("channel not closed after the server closed the connection")
	}
	waitGoroutines(t, base)
}

func TestWatchDialHonoursContext(t *testing.T) {
	// Accepts the TCP connection but never answers the upgrade.
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	go func() {
		for {
			c, err := l.Accept()
			if err != nil {
				return
			}
			defer c.Close()
		}
	}()

	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	start := time.Now()
	_, err = New(WithBaseURL("http://"+l.Addr().String())).Watch(ctx, "p")
	if err == nil {
		t.Fatal("want an error from a handshake that never completes")
	}
	if elapsed := time.Since(start); elapsed > 2*time.Second {
		t.Fatalf("Watch ignored ctx: returned after %v", elapsed)
	}
	var e *Error
	if !errors.As(err, &e) || !strings.Contains(e.Message, context.DeadlineExceeded.Error()) {
		t.Fatalf("want a transport error naming the deadline, got %v", err)
	}
}

func TestWatchRejectsOversizedFrame(t *testing.T) {
	serverSawClose := make(chan error, 1)
	srv := fakeWS(t, func(conn net.Conn, r *bufio.Reader) {
		_, _ = readClientFrame(r)
		// A text frame header claiming 1 TiB; no payload follows.
		hdr := []byte{0x81, 127}
		hdr = binary.BigEndian.AppendUint64(hdr, 1<<40)
		_, _ = conn.Write(hdr)
		// The client must hang up rather than wait for (or allocate) the
		// payload: its close frame, then EOF, is what a correct client shows.
		_ = conn.SetReadDeadline(time.Now().Add(3 * time.Second))
		var err error
		for err == nil {
			_, err = readClientFrame(r)
		}
		serverSawClose <- err
	})

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	events, err := New(WithBaseURL(srv.URL)).Watch(ctx, "p")
	if err != nil {
		t.Fatal(err)
	}
	select {
	case _, ok := <-events:
		if ok {
			t.Fatal("an oversized frame must not yield an event")
		}
	case <-time.After(3 * time.Second):
		t.Fatal("channel not closed after an oversized frame")
	}
	if err := <-serverSawClose; !errors.Is(err, net.ErrClosed) && !strings.Contains(err.Error(), "EOF") {
		t.Fatalf("client did not hang up on the oversized frame: %v", err)
	}
}
