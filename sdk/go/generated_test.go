package drsg

import (
	"os"
	"path/filepath"
	"runtime"
	"testing"

	"github.com/wangyingsm/dr-strange/sdk/go/internal/gen"
)

// The committed generated.go must match the schema (no manual drift).
func TestGeneratedIsCurrent(t *testing.T) {
	_, self, _, _ := runtime.Caller(0)
	dir := filepath.Dir(self) // sdk/go
	schema, err := os.ReadFile(filepath.Join(dir, "..", "..", "crates", "dr-strange-web", "openrpc.json"))
	if err != nil {
		t.Fatal(err)
	}
	want, err := gen.Render(schema)
	if err != nil {
		t.Fatal(err)
	}
	got, err := os.ReadFile(filepath.Join(dir, "generated.go"))
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != string(want) {
		t.Error("generated.go is stale — run `go generate ./...`")
	}
}
