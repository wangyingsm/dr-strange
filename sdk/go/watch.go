// Live change-feed subscription over a long-lived WebSocket (ROADMAP §5).
//
// Go's standard library has no WebSocket client, and this SDK carries zero
// dependencies, so a minimal RFC 6455 text-frame client is hand-rolled here:
// client frames are masked, server frames are de-fragmented, and pings are
// answered. Enough to consume the change feed, nothing more.
package drsg

import (
	"bufio"
	"context"
	"crypto/rand"
	"crypto/tls"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"io"
	"net"
	"net/url"
	"strings"
)

// Change is one node or edge that changed in a commit.
type Change struct {
	Kind string `json:"kind"` // "node" | "edge"
	Op   string `json:"op"`   // "created" | "updated" | "deleted"
	ID   uint64 `json:"id"`
	// Labels are a node's labels (nil for edges, and for a deleted node).
	Labels []string `json:"labels,omitempty"`
	// Record is the committed node/edge for a create/update (embeddings and
	// "_"-prefixed props stripped); nil for a delete — read AsOf(seq-1) for the
	// before-state.
	Record map[string]any `json:"record,omitempty"`
}

// ChangeEvent is all the changes one commit produced.
type ChangeEvent struct {
	Plane     string   `json:"plane"`
	Seq       uint64   `json:"seq"`
	Truncated bool     `json:"truncated"`
	Changes   []Change `json:"changes"`
}

type watchConfig struct{ label string }

// WatchOption configures Watch.
type WatchOption func(*watchConfig)

// WithLabel streams only node changes carrying label (edges pass unfiltered).
func WithLabel(label string) WatchOption {
	return func(w *watchConfig) { w.label = label }
}

// Watch subscribes to a plane's change feed (ROADMAP §5) over a long-lived
// WebSocket. It dials and subscribes synchronously (so a connection error is
// returned here), then streams each committed ChangeEvent on the returned
// channel until ctx is cancelled or the connection ends, at which point the
// channel is closed.
//
// Best-effort: a reader that stops draining the channel can miss commits, and
// reconnecting after a drop is the caller's to add. Cancel ctx to stop.
func (c *Client) Watch(ctx context.Context, plane string, opts ...WatchOption) (<-chan ChangeEvent, error) {
	var cfg watchConfig
	for _, o := range opts {
		o(&cfg)
	}

	ws, err := dialWebSocket(c.baseURL, c.token)
	if err != nil {
		return nil, err
	}

	sub := map[string]any{"plane": plane}
	if cfg.label != "" {
		sub["label"] = cfg.label
	}
	req, _ := json.Marshal(rpcRequest{JSONRPC: "2.0", Method: "plane.watch", Params: sub})
	if err := ws.writeText(req); err != nil {
		ws.close()
		return nil, &Error{Code: -32000, Message: "websocket subscribe failed: " + err.Error()}
	}

	// Closing the conn on ctx.Done unblocks the read loop below.
	go func() {
		<-ctx.Done()
		ws.close()
	}()

	out := make(chan ChangeEvent)
	go func() {
		defer close(out)
		defer ws.close()
		for {
			msg, err := ws.readText()
			if err != nil {
				return
			}
			var envelope struct {
				Method string      `json:"method"`
				Params ChangeEvent `json:"params"`
			}
			if json.Unmarshal(msg, &envelope) != nil || envelope.Method != "plane.change" {
				continue
			}
			select {
			case out <- envelope.Params:
			case <-ctx.Done():
				return
			}
		}
	}()
	return out, nil
}

// ---- minimal RFC 6455 client ----------------------------------------------

type wsConn struct {
	conn net.Conn
	r    *bufio.Reader
}

// dialWebSocket opens a WebSocket to <baseURL>/ws, carrying the token in the
// query string (browsers can't set a header on the handshake, and the server
// reads ?token= there).
func dialWebSocket(baseURL, token string) (*wsConn, error) {
	u, err := url.Parse(baseURL)
	if err != nil {
		return nil, &Error{Code: -32000, Message: "bad base URL: " + err.Error()}
	}
	secure := u.Scheme == "https"
	host := u.Host
	if u.Port() == "" {
		if secure {
			host += ":443"
		} else {
			host += ":80"
		}
	}

	var conn net.Conn
	if secure {
		conn, err = tls.Dial("tcp", host, nil)
	} else {
		conn, err = net.Dial("tcp", host)
	}
	if err != nil {
		return nil, &Error{Code: -32000, Message: "connection failed: " + err.Error()}
	}

	path := "/ws"
	if token != "" {
		path += "?token=" + url.QueryEscape(token)
	}
	var keyBytes [16]byte
	_, _ = rand.Read(keyBytes[:])
	key := base64.StdEncoding.EncodeToString(keyBytes[:])
	handshake := "GET " + path + " HTTP/1.1\r\n" +
		"Host: " + u.Host + "\r\n" +
		"Upgrade: websocket\r\n" +
		"Connection: Upgrade\r\n" +
		"Sec-WebSocket-Key: " + key + "\r\n" +
		"Sec-WebSocket-Version: 13\r\n\r\n"
	if _, err := conn.Write([]byte(handshake)); err != nil {
		conn.Close()
		return nil, &Error{Code: -32000, Message: "handshake write failed: " + err.Error()}
	}

	r := bufio.NewReader(conn)
	status, err := r.ReadString('\n')
	if err != nil || !strings.Contains(status, " 101 ") {
		conn.Close()
		return nil, &Error{Code: -32000, Message: "websocket upgrade refused: " + strings.TrimSpace(status)}
	}
	// Drain the rest of the response headers.
	for {
		line, err := r.ReadString('\n')
		if err != nil {
			conn.Close()
			return nil, &Error{Code: -32000, Message: "handshake read failed: " + err.Error()}
		}
		if line == "\r\n" || line == "\n" {
			break
		}
	}
	return &wsConn{conn: conn, r: r}, nil
}

func (w *wsConn) writeText(payload []byte) error { return w.writeFrame(0x1, payload) }

func (w *wsConn) writeFrame(opcode byte, payload []byte) error {
	header := []byte{0x80 | opcode}
	n := len(payload)
	switch {
	case n < 126:
		header = append(header, 0x80|byte(n))
	case n < 65536:
		header = append(header, 0x80|126)
		header = binary.BigEndian.AppendUint16(header, uint16(n))
	default:
		header = append(header, 0x80|127)
		header = binary.BigEndian.AppendUint64(header, uint64(n))
	}
	var mask [4]byte
	_, _ = rand.Read(mask[:])
	header = append(header, mask[:]...)
	masked := make([]byte, n)
	for i, b := range payload {
		masked[i] = b ^ mask[i%4]
	}
	_, err := w.conn.Write(append(header, masked...))
	return err
}

// readText returns the next complete text message, answering pings and
// following continuation frames. Returns an error (io.EOF on close).
func (w *wsConn) readText() ([]byte, error) {
	var message []byte
	for {
		var h [2]byte
		if _, err := io.ReadFull(w.r, h[:]); err != nil {
			return nil, err
		}
		fin := h[0]&0x80 != 0
		opcode := h[0] & 0x0F
		masked := h[1]&0x80 != 0
		length := uint64(h[1] & 0x7F)
		switch length {
		case 126:
			var b [2]byte
			if _, err := io.ReadFull(w.r, b[:]); err != nil {
				return nil, err
			}
			length = uint64(binary.BigEndian.Uint16(b[:]))
		case 127:
			var b [8]byte
			if _, err := io.ReadFull(w.r, b[:]); err != nil {
				return nil, err
			}
			length = binary.BigEndian.Uint64(b[:])
		}
		var mask [4]byte
		if masked {
			if _, err := io.ReadFull(w.r, mask[:]); err != nil {
				return nil, err
			}
		}
		data := make([]byte, length)
		if _, err := io.ReadFull(w.r, data); err != nil {
			return nil, err
		}
		if masked {
			for i := range data {
				data[i] ^= mask[i%4]
			}
		}
		switch opcode {
		case 0x8: // close
			return nil, io.EOF
		case 0x9: // ping → pong
			if err := w.writeFrame(0xA, data); err != nil {
				return nil, err
			}
		case 0xA: // pong
		default: // 0x1 text or 0x0 continuation
			message = append(message, data...)
			if fin {
				return message, nil
			}
		}
	}
}

func (w *wsConn) close() {
	_ = w.writeFrame(0x8, nil) // best-effort close frame
	_ = w.conn.Close()
}
