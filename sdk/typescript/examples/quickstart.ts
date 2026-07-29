// Minimal dr-strange quickstart — run against a `drsg serve` on :7700.
//   DRSG_TOKEN=… bun examples/quickstart.ts
import { Drsg } from "drsg";

const db = new Drsg(); // base http://127.0.0.1:7700; token from $DRSG_TOKEN

await db.nodeCreate({ plane: "startup", key: "alice", labels: ["Person"] });
await db.nodeCreate({ plane: "startup", key: "bob", labels: ["Person"] });
await db.edgeCreate({ plane: "startup", src: "alice", dst: "bob", type: "KNOWS" });
await db.nodeUpdate({ plane: "startup", key: "alice", set: { age: 30 } });

const alice = await db.nodeGet({ plane: "startup", key: "alice" });
console.log(`alice.age = ${alice?.properties.age}`);

const stats = await db.dbStats();
console.log(`${stats.nodes} nodes, ${stats.edges} edge(s)`);
