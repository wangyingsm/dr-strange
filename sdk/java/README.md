# drsg — Java client for dr-strange

A client for a `drsg serve` JSON-RPC endpoint. The method surface and its types
are **generated from the server's OpenRPC schema**
(`crates/dr-strange-web/openrpc.json`), so they always match the wire protocol.
HTTP uses the JDK `HttpClient`; JSON uses Jackson (the one runtime dependency).
Targets Java 17.

## Install

```xml
<dependency>
  <groupId>io.github.wangyingsm</groupId>
  <artifactId>drsg</artifactId>
  <version>0.1.0</version>
</dependency>
```

## Use

```java
import io.github.wangyingsm.drsg.Drsg;
import io.github.wangyingsm.drsg.DrsgException;

// base URL defaults to http://127.0.0.1:7700; token defaults to $DRSG_TOKEN
Drsg db = new Drsg("http://127.0.0.1:7700", "…");

db.nodeCreate(Drsg.NodeCreateParams.of("startup").withKey("alice").withLabels(List.of("Person")));
db.nodeCreate(Drsg.NodeCreateParams.of("startup").withKey("bob").withLabels(List.of("Person")));
db.edgeCreate(Drsg.EdgeCreateParams.of("startup", "alice", "bob", "KNOWS"));

db.nodeUpdate(Drsg.NodeUpdateParams.of("startup").withKey("alice").withSet(Map.of("age", 41)));
Drsg.NodeRecord alice = db.nodeGet(Drsg.NodeGetParams.of("startup").withKey("alice")); // null if absent
Drsg.DbStats stats = db.dbStats();
```

Each method is the RPC method camelCased (`node.create` → `nodeCreate`,
`plane.set_props` → `planeSetProps`), taking a typed `…Params` record and
returning the typed result. Build params with `Params.of(required…)` plus a
`withX(…)` per optional field; a node reference (`src`/`dst`) is a `Long` id or
a `String` key. Types are nested in the `Drsg` class (`Drsg.NodeRecord`).

A runnable version is [`examples/Quickstart.java`](examples/Quickstart.java) (compile with the built classes + Jackson on the classpath).

### Auth

The whole surface is authenticated. Pass a token to the constructor or set
`DRSG_TOKEN`; it rides each request as `Authorization: Bearer …`. On a
missing/invalid credential the call throws `DrsgAuthException` (a
`DrsgException` with `code() == -32001`).

## Discover

`db.rpcDiscover()` returns the server's live OpenRPC document.

## Develop

The client is generated. After editing the schema:

```bash
cd sdk/java
./mvnw -q compile exec:java     # regenerate src/.../Drsg.java
./mvnw test                     # spins up a real drsg serve (needs the built binary)
```

`GeneratedDriftTest` fails if the committed `Drsg.java` has drifted from the
schema. The e2e suite skips (does not fail) if no `drsg` binary is found; point
it at one with `$DRSG_BIN`, or build with `cargo build -p dr-strange-cli`.

> Built with Maven (not Gradle): the installed JDK 25 is newer than Gradle's
> bundled Kotlin/Groovy can parse, whereas Maven runs directly on the JVM.
