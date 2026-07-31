// End-to-end tests: drive a real `drsg serve` over the client.
//
// Requires the `drsg` binary. Point at it with $DRSG_BIN, else the workspace
// target/{debug,release}/drsg is used; the suite skips if none is found.
package drsg

import (
	"context"
	"errors"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"testing"
	"time"
)

const testToken = "test-token"

func ptr[T any](v T) *T { return &v }

func findBinary() string {
	if env := os.Getenv("DRSG_BIN"); env != "" {
		if _, err := os.Stat(env); err == nil {
			return env
		}
		return ""
	}
	_, self, _, _ := runtime.Caller(0)
	root := filepath.Join(filepath.Dir(self), "..", "..") // repo root
	for _, profile := range []string{"debug", "release"} {
		cand := filepath.Join(root, "target", profile, "drsg")
		if _, err := os.Stat(cand); err == nil {
			return cand
		}
	}
	return ""
}

func freePort(t *testing.T) int {
	t.Helper()
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port
}

// serve spins up a token-gated `drsg serve` and returns a client pointed at it.
// It skips the test if no binary is available.
func serve(t *testing.T) *Client {
	t.Helper()
	bin := findBinary()
	if bin == "" {
		t.Skip("drsg binary not found; run `cargo build -p dr-strange-cli`")
	}
	port := freePort(t)
	addr := "127.0.0.1:" + strconv.Itoa(port)
	db := filepath.Join(t.TempDir(), "sdk-test.drsg")

	cmd := exec.Command(bin, "--db", db, "serve", "--addr", addr)
	cmd.Env = append(os.Environ(), "DRSG_TOKEN="+testToken)
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = cmd.Process.Kill()
		_, _ = cmd.Process.Wait()
	})

	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if c, err := net.DialTimeout("tcp", addr, 100*time.Millisecond); err == nil {
			_ = c.Close()
			return New(WithBaseURL("http://"+addr), WithToken(testToken))
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatal("server never started listening")
	return nil
}

func TestCRUDRoundtrip(t *testing.T) {
	db := serve(t)
	ctx := context.Background()

	stats, err := db.DbStats(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if stats.Nodes != 0 {
		t.Fatalf("want 0 nodes, got %d", stats.Nodes)
	}

	alice, err := db.NodeCreate(ctx, NodeCreateParams{Plane: "startup", Key: ptr("alice"), Labels: []string{"Person"}})
	if err != nil {
		t.Fatal(err)
	}
	if alice.ExternalKey == nil || *alice.ExternalKey != "alice" {
		t.Fatalf("external_key = %v", alice.ExternalKey)
	}
	if _, err := db.NodeCreate(ctx, NodeCreateParams{Plane: "startup", Key: ptr("bob"), Labels: []string{"Person"}}); err != nil {
		t.Fatal(err)
	}

	edge, err := db.EdgeCreate(ctx, EdgeCreateParams{Plane: "startup", Src: "alice", Dst: "bob", Type: "KNOWS"})
	if err != nil {
		t.Fatal(err)
	}
	if edge.Type != "KNOWS" {
		t.Fatalf("edge type = %q", edge.Type)
	}

	// Property patch: set then unset, with types preserved.
	upd, err := db.NodeUpdate(ctx, NodeUpdateParams{Plane: "startup", Key: ptr("alice"), Set: Properties{"age": 41, "city": "NYC"}})
	if err != nil {
		t.Fatal(err)
	}
	if upd.Properties["age"] != float64(41) {
		t.Fatalf("age = %v", upd.Properties["age"])
	}
	upd, err = db.NodeUpdate(ctx, NodeUpdateParams{Plane: "startup", Key: ptr("alice"), Unset: []string{"city"}})
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := upd.Properties["city"]; ok {
		t.Fatal("city was not unset")
	}

	got, err := db.NodeGet(ctx, NodeGetParams{Plane: "startup", Key: ptr("alice")})
	if err != nil {
		t.Fatal(err)
	}
	if got == nil || got.Properties["age"] != float64(41) {
		t.Fatalf("get: %+v", got)
	}

	// Delete cascades the edge; the graph is left consistent.
	del, err := db.NodeDelete(ctx, NodeDeleteParams{Plane: "startup", Key: ptr("alice")})
	if err != nil {
		t.Fatal(err)
	}
	if !del.Deleted {
		t.Fatal("node not deleted")
	}
	stats, err = db.DbStats(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if stats.Nodes != 1 || stats.Edges != 0 {
		t.Fatalf("after delete: %d nodes, %d edges", stats.Nodes, stats.Edges)
	}
}

func TestPlaneAdmin(t *testing.T) {
	db := serve(t)
	ctx := context.Background()

	p, err := db.PlaneCreate(ctx, PlaneCreateParams{Name: "notes"})
	if err != nil {
		t.Fatal(err)
	}
	if p.Name != "notes" {
		t.Fatalf("created name = %q", p.Name)
	}
	r, err := db.PlaneRename(ctx, PlaneRenameParams{Plane: "notes", To: "archive"})
	if err != nil {
		t.Fatal(err)
	}
	if r.Name != "archive" {
		t.Fatalf("renamed name = %q", r.Name)
	}
	d, err := db.PlaneDelete(ctx, PlaneDeleteParams{Plane: "archive"})
	if err != nil {
		t.Fatal(err)
	}
	if !d.Deleted {
		t.Fatal("plane not deleted")
	}
}

func TestDiscover(t *testing.T) {
	db := serve(t)
	doc, err := db.RpcDiscover(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if doc["openrpc"] != "1.2.6" {
		t.Fatalf("openrpc = %v", doc["openrpc"])
	}
}

func TestBadTokenIsAuthError(t *testing.T) {
	good := serve(t)
	bad := New(WithBaseURL(good.baseURL), WithToken("wrong"))
	_, err := bad.DbStats(context.Background())
	if err == nil {
		t.Fatal("expected an error")
	}
	if !IsAuthError(err) {
		t.Fatalf("want auth error, got %v", err)
	}
	var e *Error
	if !errors.As(err, &e) || e.Code != AuthErrorCode {
		t.Fatalf("want *Error code %d, got %v", AuthErrorCode, err)
	}
}

func TestWatchChangeFeed(t *testing.T) {
	db := serve(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	events, err := db.Watch(ctx, "startup", WithLabel("Widget"))
	if err != nil {
		t.Fatal(err)
	}
	time.Sleep(300 * time.Millisecond) // let the server register the subscription

	if _, err := db.NodeCreate(ctx, NodeCreateParams{
		Plane: "startup", Key: ptr("ws-widget"), Labels: []string{"Widget"},
	}); err != nil {
		t.Fatal(err)
	}

	select {
	case ev := <-events:
		if ev.Seq == 0 {
			t.Fatalf("want a non-zero seq, got %d", ev.Seq)
		}
		var found *Change
		for i := range ev.Changes {
			if rec := ev.Changes[i].Record; rec != nil && rec["external_key"] == "ws-widget" {
				found = &ev.Changes[i]
			}
		}
		if found == nil {
			t.Fatalf("change for ws-widget not in event: %+v", ev)
		}
		if found.Kind != "node" || found.Op != "created" {
			t.Fatalf("kind=%q op=%q", found.Kind, found.Op)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("no change event received over the websocket")
	}
}
