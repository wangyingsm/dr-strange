# drsg — Java client for dr-strange

A client for a `drsg serve` JSON-RPC endpoint. The method surface and its types
are **generated from the server's OpenRPC schema**
(`crates/dr-strange-web/openrpc.json`), so they always match the wire protocol.
HTTP uses the JDK `HttpClient`; JSON uses Jackson (the one runtime dependency).
Targets Java 17.

## Install

The artifact is not on Maven Central yet. Build it from a checkout of this
repository into your local Maven repository, then depend on it as usual:

```bash
cd sdk/java && ./mvnw -q -DskipTests install
```

```xml
<dependency>
  <groupId>io.github.wangyingsm</groupId>
  <artifactId>drsg</artifactId>
  <version>2.7.0</version>
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

### Lifecycle and configuration

A client is `AutoCloseable`. It owns the threads behind its JDK `HttpClient`
and any open change-feed `Subscription`, so close it when done (or use
try-with-resources):

```java
try (Drsg db = new Drsg("http://127.0.0.1:7700", token)) {
    db.dbStats();
}
```

`Client.Options` sets the per-call timeout (default 30 s, also bounding the
WebSocket handshake of `watch`) and lets several clients share one
`HttpClient` — its connection pool, proxy and TLS settings — which `close()`
then leaves running:

```java
var options = new Client.Options()
        .baseUrl("http://127.0.0.1:7700")
        .token(token)
        .timeout(Duration.ofSeconds(5))
        .httpClient(sharedHttpClient);
try (Drsg db = new Drsg(options)) {
    // typed methods as usual
}
```

### Change feed

`db.watch(plane, label, listener)` returns a `Subscription`; `close()` sends a
close frame and then aborts the socket, so it returns promptly even if the peer
never answers the close handshake. The listener runs on the `HttpClient`'s
executor; if it throws, the exception is logged at `WARNING` under the logger
`io.github.wangyingsm.drsg.Client` (`System.Logger`, so java.util.logging by
default) and the subscription continues.

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
