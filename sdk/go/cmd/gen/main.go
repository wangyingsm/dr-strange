// Command gen regenerates generated.go from the OpenRPC schema.
//
// Run via `go generate ./...` from the module root, or directly with
// `go run ./cmd/gen`. Paths are resolved relative to this source file, so the
// working directory does not matter.
package main

import (
	"log"
	"os"
	"path/filepath"
	"runtime"

	"github.com/wangyingsm/dr-strange/sdk/go/internal/gen"
)

func main() {
	_, self, _, ok := runtime.Caller(0)
	if !ok {
		log.Fatal("cannot resolve source path")
	}
	moduleRoot := filepath.Join(filepath.Dir(self), "..", "..") // sdk/go
	schemaPath := filepath.Join(moduleRoot, "..", "..", "crates", "dr-strange-web", "openrpc.json")
	outPath := filepath.Join(moduleRoot, "generated.go")

	schema, err := os.ReadFile(schemaPath)
	if err != nil {
		log.Fatalf("read schema: %v", err)
	}
	src, err := gen.Render(schema)
	if err != nil {
		log.Fatalf("render: %v", err)
	}
	if err := os.WriteFile(outPath, src, 0o644); err != nil {
		log.Fatalf("write %s: %v", outPath, err)
	}
	log.Printf("wrote %s", outPath)
}
