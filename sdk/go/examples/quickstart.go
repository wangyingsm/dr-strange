// Minimal dr-strange quickstart — run against a `drsg serve` on :7700.
//
//	DRSG_TOKEN=… go run ./examples
package main

import (
	"context"
	"fmt"
	"log"

	drsg "github.com/wangyingsm/dr-strange/sdk/go"
)

func ptr[T any](v T) *T { return &v }

func must(_ any, err error) {
	if err != nil {
		log.Fatal(err)
	}
}

func main() {
	ctx := context.Background()
	db := drsg.New() // base http://127.0.0.1:7700; token from $DRSG_TOKEN

	must(db.NodeCreate(ctx, drsg.NodeCreateParams{Plane: "startup", Key: ptr("alice"), Labels: []string{"Person"}}))
	must(db.NodeCreate(ctx, drsg.NodeCreateParams{Plane: "startup", Key: ptr("bob"), Labels: []string{"Person"}}))
	must(db.EdgeCreate(ctx, drsg.EdgeCreateParams{Plane: "startup", Src: "alice", Dst: "bob", Type: "KNOWS"}))
	must(db.NodeUpdate(ctx, drsg.NodeUpdateParams{Plane: "startup", Key: ptr("alice"), Set: drsg.Properties{"age": 30}}))

	alice, err := db.NodeGet(ctx, drsg.NodeGetParams{Plane: "startup", Key: ptr("alice")})
	if err != nil {
		log.Fatal(err)
	}
	fmt.Printf("alice.age = %v\n", alice.Properties["age"])

	stats, err := db.DbStats(ctx)
	if err != nil {
		log.Fatal(err)
	}
	fmt.Printf("%d nodes, %d edge(s)\n", stats.Nodes, stats.Edges)
}
