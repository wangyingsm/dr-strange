//! End-to-end proof of the MCP endpoint (ROADMAP §10): a real `drsg serve` on
//! an ephemeral port, driven by real MCP clients over Streamable HTTP — not
//! raw HTTP requests, but `rmcp`'s own client transport, the way an agent
//! host actually talks to it. Proves the thing raw-HTTP smoke tests can't:
//! two independent MCP sessions (simulating two agent hosts, e.g. Claude
//! Code and Codex) reading and writing through `/mcp` see one shared
//! `Database`, not two private ones — the whole point of hosting the tool
//! set on `serve` instead of each host embedding its own.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use dr_strange_core::{Database, Properties};
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "test-mcp-token";

fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// Sets `DRSG_TOKEN` exactly once, before this binary starts any thread that
/// might read it.
///
/// The tests in this file are separate `#[tokio::test]`s but share one
/// process, and each spawns a server thread that calls `SharedToken::from_env`
/// / `AllowedOrigins::from_env`. A per-test `set_var` would therefore run
/// concurrently with another test's server thread calling `getenv` — under
/// glibc `setenv` may reallocate `environ`, so that is a real data race, and
/// the reason edition 2024 marks the function `unsafe`. It would surface as
/// an occasional segfault or as a server that reads no token and 401s
/// everything.
///
/// `Once` removes the race rather than documenting it: the first caller sets
/// the variable and every later caller blocks until that is finished, so the
/// single write happens-before every server thread this file creates.
///
/// SAFETY: no other thread exists that could be reading the environment —
/// `call_once` serialises the writers, and the only readers are the server
/// threads spawned below, after it returns.
fn set_token_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe { std::env::set_var("DRSG_TOKEN", TOKEN) });
}

/// Spins up a real server with a seeded node, gated by `DRSG_TOKEN` — the
/// realistic configuration for a programmatic client (ROADMAP §10's "token
/// posture" fork: with no token, `/mcp` refuses even reads, same as `/rpc`).
fn spawn_server() -> SocketAddr {
    set_token_once();
    let addr = free_addr();
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    txn.create_node_with_key("alice", &["Person"], Properties::new())
        .unwrap();
    txn.commit().unwrap();

    std::thread::spawn(move || {
        let opts = dr_strange_web::ServeOptions {
            addr,
            ..Default::default()
        };
        dr_strange_web::serve(db, None, opts).unwrap();
    });
    addr
}

/// Poll until the server accepts connections.
async fn wait_ready(addr: SocketAddr) {
    for _ in 0..80 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server never started listening");
}

type Client = RunningService<RoleClient, ClientConfig>;

/// A fresh MCP client session against `/mcp` — a new transport and a new
/// `initialize` handshake, simulating a separate agent host process rather
/// than reusing one connection for every call.
async fn connect(addr: SocketAddr, token: &str) -> anyhow::Result<Client> {
    let config = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
        .auth_header(token);
    let transport = StreamableHttpClientTransport::from_config(config);
    Ok(ClientConfig::default().serve(transport).await?)
}

async fn call(client: &Client, tool: &str, args: Value) -> Value {
    let args = match args {
        Value::Object(m) => m,
        Value::Null => serde_json::Map::new(),
        other => panic!("tool args must be an object, got {other}"),
    };
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{tool} failed: {result:?}");
    let text = &result.content[0].as_text().unwrap().text;
    serde_json::from_str(text).unwrap()
}

/// `drsg-mcp`'s relay: a host speaking stdio reaches the database this
/// running server holds, rather than opening one of its own.
///
/// The whole point of the shortcut, end to end — a `.mcp.json` naming this
/// server, the liveness probe, and the message pump — proved the way it is
/// actually used: an MCP client on one end of a pipe, the relay on the other,
/// and a tool call that can only be answered by *this* server's data (the
/// seeded `alice`, which a freshly opened database would not have).
#[tokio::test]
async fn the_stdio_relay_serves_the_running_servers_database() {
    let addr = spawn_server();
    wait_ready(addr).await;

    // What `drsg init` writes, in a repository the relay then discovers from.
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "drsg-watch": {
                    "type": "http",
                    "url": format!("http://{addr}/mcp"),
                    "headers": { "Authorization": format!("Bearer {TOKEN}") },
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let upstream = dr_strange_mcp::relay::discover(repo.path()).expect("declared by .mcp.json");
    // A proxy that could not carry anything: port 1 refuses at once. Both the
    // probe and the relay below go to a loopback server, which is never
    // proxied, so this must make no difference — and before issue #84 was
    // fixed it made all of it fail (an operator with `http_proxy` set got a
    // probe that never arrived and a silent fallback to a different graph).
    let net = dr_strange_net::Network::through(
        "http://127.0.0.1:1".parse().unwrap(),
        dr_strange_net::NoProxy::default(),
    );
    assert!(
        dr_strange_mcp::relay::alive(&upstream, Duration::from_secs(2), &net).await,
        "the probe must find a server that is plainly answering",
    );

    // A pipe stands in for the host's stdio: the relay serves one end, an MCP
    // client drives the other.
    let (host_side, relay_side) = tokio::io::duplex(64 * 1024);
    let relayed = tokio::spawn(async move {
        let (rx, tx) = tokio::io::split(relay_side);
        dr_strange_mcp::relay::relay_over(
            rmcp::transport::async_rw::AsyncRwTransport::new_server(rx, tx),
            &upstream,
            &net,
        )
        .await
    });

    let (rx, tx) = tokio::io::split(host_side);
    let client: Client = ClientConfig::default()
        .serve(rmcp::transport::async_rw::AsyncRwTransport::new_client(
            rx, tx,
        ))
        .await
        .expect("the relay carries the initialize handshake");

    // Tools are the upstream's, listed through the relay.
    let tools = client.list_tools(Default::default()).await.unwrap();
    assert!(
        tools.tools.iter().any(|t| t.name == "get_node"),
        "the server's own tool set reaches the host"
    );
    // An agent told "the plugins changed since this plane was built" needs a way
    // to act on it without leaving the conversation (issue #39).
    assert!(
        tools.tools.iter().any(|t| t.name == "rebuild"),
        "rebuild is advertised: {:?}",
        tools.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    // And the data is the server's: `alice` exists only in the database this
    // process seeded, never in one the relay could have opened itself.
    let node = call(&client, "get_node", json!({ "key": "alice" })).await;
    assert_eq!(node["external_key"], "alice");

    // Closing the host ends the relay, which is how the process exits when a
    // host disconnects.
    client.cancel().await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), relayed)
        .await
        .expect("the relay outlived its host");
    ended.unwrap().expect("the relay ended cleanly");
}

/// The agent verbs and the query tool over `/mcp`, in the shapes an agent
/// gets them: `cypher` answers a projecting query with a table, and `fathom`
/// answers with the compact text every agent verb speaks.
#[tokio::test]
async fn tools_answer_in_the_shape_the_question_asked_for() {
    let addr = spawn_server();
    wait_ready(addr).await;
    let client = connect(addr, TOKEN).await.unwrap();

    // A projection: columns and rows, not nodes.
    let table = call(
        &client,
        "cypher",
        json!({ "query": "MATCH (n:Person) RETURN key(n) AS who, count(*) AS n" }),
    )
    .await;
    assert_eq!(table["columns"], json!(["who", "n"]));
    assert_eq!(table["rows"], json!([["alice", 1]]));

    // The same tool without a projection still answers with records.
    let nodes = call(
        &client,
        "cypher",
        json!({ "query": "MATCH (n:Person) RETURN n" }),
    )
    .await;
    assert_eq!(nodes[0]["external_key"], "alice");

    // `fathom` reports the region's makeup — here a lone node, which it says
    // rather than implying it looked further.
    // No depth: the tool's default, what an agent naming only a symbol gets.
    let region = call(&client, "fathom", json!({ "name": "alice" })).await;
    let text = region.as_str().expect("fathom answers with text");
    assert!(
        text.contains("region: 0 hops out and in — 1 nodes, 0 edges"),
        "{text}"
    );
    assert!(text.contains("Person 1"), "{text}");
    assert!(text.contains("nothing further connects"), "{text}");
    assert!(
        text.contains("short of the 2"),
        "depth defaults to 2: {text}"
    );

    // `wire` is reachable and says what it found, which here is nothing: this
    // plane holds one Person and no plugin that states routes.
    let routes = call(&client, "wire", json!({ "name": "/bridge/check" })).await;
    let text = routes.as_str().expect("wire answers with text");
    assert!(text.contains("no routes recorded in this plane"), "{text}");

    client.cancel().await.unwrap();
}

/// Attaching on a **running** server, which is the point of the tool: a plane
/// this one may ask, recorded and in force without a restart or a config file.
#[tokio::test]
async fn a_plane_is_attached_on_a_running_server() {
    let addr = spawn_server();
    wait_ready(addr).await;
    let client = connect(addr, TOKEN).await.unwrap();

    call(&client, "create_plane", json!({ "name": "ui" })).await;
    let made = call(&client, "attach", json!({ "name": "ui" })).await;
    assert_eq!(
        made.as_str().unwrap(),
        "attached on plane 'startup'\n  ui  \u{2192} plane 'ui' on this instance\n"
    );

    // A URL is recorded as given; nothing is contacted until a verb asks it.
    let remote = call(
        &client,
        "attach",
        json!({ "name": "front", "url": "http://127.0.0.1:1", "as": "web" }),
    )
    .await;
    assert!(
        remote
            .as_str()
            .unwrap()
            .contains("front  \u{2192} plane 'web' at http://127.0.0.1:1   not contacted"),
        "{remote}"
    );

    let listed = call(&client, "attachments", json!({})).await;
    let text = listed.as_str().unwrap();
    assert!(text.starts_with("plane 'startup' attaches 2"), "{text}");

    // A local attachment is verified, and the refusal names what is there.
    let refused = client
        .call_tool(
            CallToolRequestParams::new("attach".to_string())
                .with_arguments(json!({ "name": "nope" }).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_eq!(refused.is_error, Some(true), "{refused:?}");
    let why = &refused.content[0].as_text().unwrap().text;
    assert!(why.contains("it holds: startup, ui"), "{why}");

    let gone = call(&client, "detach", json!({ "name": "ui" })).await;
    assert!(gone.as_str().unwrap().starts_with("detached"), "{gone}");

    client.cancel().await.unwrap();
}

/// One route as a language plugin writes it: the symbol, which side of the
/// boundary it is on (`serves` or `requests`), and the route itself.
struct States {
    key: &'static str,
    side: &'static str,
    route: &'static str,
    file: &'static str,
}

/// A second real server, holding `plane` with the routes `states` names —
/// the other repository, on its own drsg, which is the case issue #42 is
/// about. Gated by the same `DRSG_TOKEN`: attached graphs are one
/// environment, so the asking server presents the token it already holds.
fn spawn_stating(plane: &'static str, states: &'static [States]) -> SocketAddr {
    set_token_once();
    let addr = free_addr();
    let db = Database::in_memory().unwrap();
    let handle = db.create_plane(plane, Properties::new()).unwrap();
    let mut txn = handle.write().unwrap();
    for s in states {
        let mut props = Properties::new();
        props.insert(
            "file".into(),
            dr_strange_core::PropDesc::new(dr_strange_core::PropValue::Str(s.file.into())),
        );
        props.insert(
            "line".into(),
            dr_strange_core::PropDesc::new(dr_strange_core::PropValue::Int(3)),
        );
        props.insert(
            s.side.into(),
            dr_strange_core::PropDesc::new(dr_strange_core::PropValue::List(vec![
                dr_strange_core::PropValue::Str(s.route.into()),
            ])),
        );
        txn.create_node_with_key(s.key, &["Function"], props)
            .unwrap();
    }
    txn.commit().unwrap();

    std::thread::spawn(move || {
        let opts = dr_strange_web::ServeOptions {
            addr,
            ..Default::default()
        };
        dr_strange_web::serve(db, None, opts).unwrap();
    });
    addr
}

/// Two drsg instances, one attached to the other — the case issue #42 opens
/// with: the frontend repository and the backend repository are not one
/// graph, and the hop between them has to cross a network.
///
/// Nothing new carries the question: it travels as a serialized plan over
/// `plane.query`, a read method this server has answered for releases. And
/// nothing is reached that the attachment did not name — the address guard
/// stays on, re-permitted for exactly that host.
#[tokio::test]
async fn wire_reaches_a_plane_another_server_serves() {
    let far = spawn_stating(
        "web",
        &[
            States {
                key: "web/src/client.loadOne",
                side: "requests",
                route: "GET /api/orders/{}",
                file: "src/client.ts",
            },
            // On the far side and on no path asked about: a federated answer
            // that pulled a whole plane back would show this.
            States {
                key: "web/src/other.ping",
                side: "requests",
                route: "GET /health",
                file: "src/other.ts",
            },
        ],
    );
    let here = spawn_stating(
        "api",
        &[States {
            key: "app.orders.one",
            side: "serves",
            route: "GET /api/orders/{}",
            file: "api/orders.py",
        }],
    );
    wait_ready(far).await;
    wait_ready(here).await;
    let client = connect(here, TOKEN).await.unwrap();

    call(
        &client,
        "attach",
        json!({ "plane": "api", "name": "front", "url": format!("http://{far}"), "as": "web" }),
    )
    .await;

    let wired = call(
        &client,
        "wire",
        json!({ "plane": "api", "name": "/api/orders" }),
    )
    .await;
    let text = wired.as_str().expect("wire answers with text");
    assert!(text.starts_with("across  api, front"), "{text}");
    assert!(
        text.contains("serves") && text.contains("app.orders.one"),
        "the local handler: {text}"
    );
    // The row from the other server, named by the attachment it came through.
    assert!(
        text.contains("web/src/client.loadOne") && text.contains("in front"),
        "the remote caller: {text}"
    );
    assert!(
        text.contains("1 request(s) matched a declaration, 0 reached none"),
        "{text}"
    );
    // Asked for routes, the far plane answered with routes — `/health` is on
    // it and is not on the path asked about, so it is not in this answer.
    assert!(!text.contains("other.ping"), "{text}");

    client.cancel().await.unwrap();
}

/// An attachment that cannot be read is named with the reason, never skipped
/// and never fatal: the local answer still stands, and the reader is told
/// exactly which graph is missing from it.
#[tokio::test]
async fn wire_names_an_attachment_it_could_not_reach() {
    let addr = spawn_stating(
        "api",
        &[States {
            key: "app.orders.one",
            side: "serves",
            route: "GET /api/orders/{}",
            file: "api/orders.py",
        }],
    );
    wait_ready(addr).await;
    let client = connect(addr, TOKEN).await.unwrap();

    call(
        &client,
        "attach",
        json!({ "plane": "api", "name": "front", "url": "http://127.0.0.1:1", "as": "web" }),
    )
    .await;
    let wired = call(
        &client,
        "wire",
        json!({ "plane": "api", "name": "/api/orders" }),
    )
    .await;
    let text = wired.as_str().unwrap();
    assert!(text.contains("not read  front"), "{text}");
    assert!(
        text.contains("http://127.0.0.1:1/rpc"),
        "the url it tried: {text}"
    );
    // The local side is still answered.
    assert!(text.contains("app.orders.one"), "{text}");

    client.cancel().await.unwrap();
}

/// A library and a repository depending on it, each on its own real server —
/// the case issue #43 opens with, where a shared library cannot say which
/// business repositories a change in it reaches.
///
/// `declares` is what this repository's own manifest publishes; `depends_on`
/// is a coordinate it declares a dependency on; `standin` is a foreign symbol
/// its source references, which the plugins record as `External`.
struct Repo {
    plane: &'static str,
    declares: &'static str,
    depends_on: Option<&'static str>,
    standin: Option<&'static str>,
    symbol: &'static str,
}

/// A real server holding `repo` as the plugins would have written it.
fn spawn_repo(repo: &'static Repo) -> SocketAddr {
    use dr_strange_core::{PropDesc, PropValue};
    set_token_once();
    let addr = free_addr();
    let db = Database::in_memory().unwrap();
    let handle = db.create_plane(repo.plane, Properties::new()).unwrap();
    let mut txn = handle.write().unwrap();
    let text = |v: &str| PropDesc::new(PropValue::Str(v.into()));

    let mut props = Properties::new();
    props.insert("declares".into(), text(repo.declares));
    let manifest = txn
        .create_node_with_key("pom.xml", &["Manifest"], props)
        .unwrap();
    if let Some(coordinate) = repo.depends_on {
        let dep = txn
            .create_node_with_key(coordinate, &["Package", "External"], Properties::new())
            .unwrap();
        let mut edge = Properties::new();
        edge.insert("version".into(), text("2.4.1"));
        edge.insert("scope".into(), text("runtime"));
        txn.create_edge(manifest, dep, "DEPENDS_ON", edge).unwrap();
    }
    let mut props = Properties::new();
    props.insert("file".into(), text("src/Thing.java"));
    props.insert("line".into(), PropDesc::new(PropValue::Int(5)));
    let symbol = txn
        .create_node_with_key(repo.symbol, &["Class"], props)
        .unwrap();
    if let Some(key) = repo.standin {
        let foreign = txn
            .create_node_with_key(key, &["Class", "External"], Properties::new())
            .unwrap();
        txn.create_edge(symbol, foreign, "EXTENDS", Properties::new())
            .unwrap();
    }
    txn.commit().unwrap();

    std::thread::spawn(move || {
        let opts = dr_strange_web::ServeOptions {
            addr,
            ..Default::default()
        };
        dr_strange_web::serve(db, None, opts).unwrap();
    });
    addr
}

static LIBRARY: Repo = Repo {
    plane: "common",
    declares: "com.acme:common-base",
    depends_on: None,
    standin: None,
    symbol: "com.acme.common.BaseEntity",
};

static DEPENDENT: Repo = Repo {
    plane: "orders",
    declares: "svc:orders",
    depends_on: Some("com.acme:common-base"),
    standin: Some("com.acme.common.BaseEntity"),
    symbol: "svc.orders.Order",
};

/// Asked across, `impact` reaches the repository on the other server by the
/// two keys it already states — the coordinate its build declares, and the
/// symbol its source stands in for. Neither is derived here and nothing is
/// stored: the question travels over `plane.query` like `wire`'s.
#[tokio::test]
async fn impact_reaches_a_repository_another_server_serves() {
    let far = spawn_repo(&DEPENDENT);
    let here = spawn_repo(&LIBRARY);
    wait_ready(far).await;
    wait_ready(here).await;
    let client = connect(here, TOKEN).await.unwrap();

    // Without a scope the answer is this repository's own.
    let alone = call(
        &client,
        "impact",
        json!({ "plane": "common", "name": "com.acme.common.BaseEntity" }),
    )
    .await;
    assert!(!alone.as_str().unwrap().contains("across"), "{alone}");

    call(
        &client,
        "attach",
        json!({ "plane": "common", "name": "svc", "url": format!("http://{far}"), "as": "orders" }),
    )
    .await;

    let out = call(
        &client,
        "impact",
        json!({ "plane": "common", "name": "com.acme.common.BaseEntity", "across": "attached" }),
    )
    .await;
    let text = out.as_str().expect("impact answers with text");
    assert!(text.contains("across  common, svc\n"), "{text}");
    assert!(
        text.contains(&format!("in svc  at http://{far}\n")),
        "{text}"
    );
    assert!(
        text.contains("declares  com.acme:common-base"),
        "the build join: {text}"
    );
    assert!(
        text.contains("com.acme.common.BaseEntity  \u{2190} referenced"),
        "the code join: {text}"
    );
    assert!(
        text.contains("ask that drsg for the rest: `impact <key> --plane orders`"),
        "the next call: {text}"
    );
    assert!(
        text.contains("1 declare(s) a dependency, 1 reach(es) it in code"),
        "{text}"
    );

    // A word that is neither scope is refused, not read as one of them.
    let refused = client
        .call_tool(
            CallToolRequestParams::new("impact".to_string()).with_arguments(
                json!({
                    "plane": "common",
                    "name": "com.acme.common.BaseEntity",
                    "across": "everything",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    let said = format!("{refused:?}");
    assert!(said.contains("attached"), "{said}");

    client.cancel().await.unwrap();
}

/// A database on disk under `dir`, holding one plane named after the
/// repository with one keyed symbol in it — what `drsg digest` leaves behind.
fn repo_store(dir: &std::path::Path, plane: &str) -> std::path::PathBuf {
    // The repository's own tree, beside its database, with one file in it —
    // so the plane can record a `synced_root` that really exists.
    let tree = dir.join(format!("{plane}-src"));
    std::fs::create_dir_all(tree.join("src")).unwrap();
    std::fs::write(
        tree.join("src/Thing.java"),
        format!("package {plane};\n\nclass Thing {{\n  // the only {plane} marker\n}}\n"),
    )
    .unwrap();

    let path = dir.join(plane);
    let db = Database::open(&path).unwrap();
    let handle = db.create_plane(plane, Properties::new()).unwrap();
    let mut txn = handle.write().unwrap();
    let mut props = Properties::new();
    props.insert(
        "file".into(),
        dr_strange_core::PropDesc::new(dr_strange_core::PropValue::Str("src/Thing.java".into())),
    );
    props.insert(
        "line".into(),
        dr_strange_core::PropDesc::new(dr_strange_core::PropValue::Int(3)),
    );
    txn.create_node_with_key(&format!("{plane}.Thing"), &["Class"], props)
        .unwrap();
    txn.commit().unwrap();
    handle
        .update_properties(|p| {
            p.insert(
                "synced_root".into(),
                dr_strange_core::PropDesc::new(dr_strange_core::PropValue::Str(
                    tree.display().to_string(),
                )),
            );
        })
        .unwrap();
    db.save_sidecars();
    drop(db);
    path
}

/// One server, many repositories — the whole of issue #44. Two databases are
/// mounted over `/mcp` on a running server and answered per plane, by tools
/// that were never told mounts exist.
#[tokio::test]
async fn one_server_answers_tools_across_mounted_databases() {
    set_token_once();
    let tmp = tempfile::tempdir().unwrap();
    let orders = repo_store(tmp.path(), "svc-orders");
    let billing = repo_store(tmp.path(), "svc-billing");
    let addr = free_addr();
    let granted = tmp.path().to_path_buf();
    std::thread::spawn(move || {
        let opts = dr_strange_web::ServeOptions {
            addr,
            mount_root: Some(granted),
            ..Default::default()
        };
        dr_strange_web::serve(Database::in_memory().unwrap(), None, opts).unwrap();
    });
    wait_ready(addr).await;
    let client = connect(addr, TOKEN).await.unwrap();

    for path in [&orders, &billing] {
        let out = call(
            &client,
            "mount",
            json!({ "path": path.display().to_string() }),
        )
        .await;
        assert_eq!(out["skipped"][0], "startup", "{out}");
    }

    // One port, both repositories, each answered on its own database — and
    // `context` never heard of a mount.
    for plane in ["svc-orders", "svc-billing"] {
        let got = call(
            &client,
            "context",
            json!({ "plane": plane, "name": format!("{plane}.Thing") }),
        )
        .await;
        let text = got.as_str().expect("context answers with text");
        assert!(text.contains(&format!("{plane}.Thing")), "{text}");
        assert!(text.contains("src/Thing.java"), "{text}");
    }

    // `list_planes` is the instance's whole set, and the `startup` every
    // mounted database carries is not among them.
    let planes = call(&client, "list_planes", json!({})).await;
    let names: Vec<&str> = planes
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(names.contains(&"svc-orders"), "{planes}");
    assert!(names.contains(&"svc-billing"), "{planes}");
    assert_eq!(
        names.iter().filter(|n| **n == "startup").count(),
        1,
        "only the host's own: {planes}"
    );

    // The file-reading verbs reach each repository's *own* tree: a plane and
    // a tree have to agree, or every hit lands in the wrong symbol. The
    // trees are readable because they sit under the directory this server
    // was granted to mount from.
    for plane in ["svc-orders", "svc-billing"] {
        let hits = call(
            &client,
            "grep",
            json!({ "plane": plane, "pattern": "marker" }),
        )
        .await;
        let text = hits.as_str().expect("grep answers with text");
        assert!(
            text.contains(&format!("the only {plane} marker")),
            "{plane}: {text}"
        );
        // And nothing from the other repository's tree.
        let other = match plane {
            "svc-orders" => "svc-billing",
            _ => "svc-orders",
        };
        assert!(
            !text.contains(other),
            "{plane} searched {other}'s tree: {text}"
        );

        let src = call(
            &client,
            "snippet",
            json!({ "plane": plane, "name": "src/Thing.java:1-5" }),
        )
        .await;
        let src = src.as_str().unwrap();
        assert!(src.contains(&format!("package {plane}")), "{plane}: {src}");
    }

    // Unmounted, the plane is gone from this server at once.
    let gone = call(&client, "unmount", json!({ "name": "svc-billing" })).await;
    assert_eq!(gone["unmounted"], true, "{gone}");
    let after = call(&client, "list_planes", json!({})).await;
    assert!(
        !after.to_string().contains("svc-billing"),
        "still served: {after}"
    );

    client.cancel().await.unwrap();
}

/// Read one HTTP request off `stream`: its headers, then as many body bytes
/// as `Content-Length` promised./// Read one HTTP request off `stream`: its headers, then as many body bytes
/// as `Content-Length` promised.
fn drain_request(stream: &mut std::net::TcpStream) {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    let mut want = None;
    while let Ok(1) = std::io::Read::read(stream, &mut byte) {
        buf.push(byte[0]);
        if want.is_none() && buf.ends_with(b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf).to_lowercase();
            let len = head
                .split("content-length:")
                .nth(1)
                .and_then(|r| r.split("\r\n").next())
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            want = Some(buf.len() + len);
        }
        if want.is_some_and(|w| buf.len() >= w) {
            return;
        }
    }
}

/// The address guard is load-bearing, not decoration: an attachment is read
/// from the address it names and from no other.
///
/// Re-permitting that one host is what lets a peer on loopback be attached at
/// all — so the question is whether anything else still gets refused. Here the
/// named host answers with a redirect somewhere else on loopback, which is the
/// classic way an SSRF escalates (a metadata service is one `Location` away),
/// and the resolver refuses the hop rather than following it.
#[tokio::test]
async fn an_attachment_is_read_from_the_address_it_names_and_no_other() {
    let addr = spawn_stating(
        "api",
        &[States {
            key: "app.orders.one",
            side: "serves",
            route: "GET /api/orders/{}",
            file: "api/orders.py",
        }],
    );
    // A host that answers the attachment's URL and points somewhere else.
    let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
    let redirector = elsewhere.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut stream in elsewhere.incoming().take(1).flatten() {
            // Drained first: replying to a POST whose body is still being
            // written closes the socket under the writer, and the client
            // reports that broken pipe instead of the redirect.
            drain_request(&mut stream);
            let _ = std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.2:9/rpc\r\nContent-Length: 0\r\n\r\n",
            );
        }
    });
    wait_ready(addr).await;
    let client = connect(addr, TOKEN).await.unwrap();

    call(
        &client,
        "attach",
        json!({ "plane": "api", "name": "front", "url": format!("http://{redirector}"), "as": "web" }),
    )
    .await;
    let wired = call(
        &client,
        "wire",
        json!({ "plane": "api", "name": "/api/orders" }),
    )
    .await;
    let text = wired.as_str().unwrap();
    assert!(text.contains("not read  front"), "{text}");
    assert!(
        text.contains("refusing to connect to 127.0.0.2"),
        "the hop off the named address: {text}"
    );
    // And the local side still answers.
    assert!(text.contains("app.orders.one"), "{text}");

    client.cancel().await.unwrap();
}

/// A valid token but a non-loopback `Host`: refused by the transport's
/// DNS-rebinding guard, which defaults to `localhost`/`127.0.0.1`/`::1`.
/// Pinned because it is the one way `/mcp` behaves differently from every
/// other route on this server — `/rpc` answers on any hostname — and
/// docs/{en,zh}/src/mcp.md now tells people so.
#[tokio::test]
async fn mcp_endpoint_refuses_a_non_loopback_host() {
    let addr = spawn_server();
    wait_ready(addr).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .header("host", "memory.example.com")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","method":"initialize","params":{},"id":1}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

/// An operator who puts `/mcp` behind a hostname lists it: with a token
/// configured, `ServeOptions::allowed_hosts` (or `DRSG_ALLOWED_HOSTS`) is
/// answered, and a name not on the list is still refused — the allowlist
/// widens the guard, it does not switch it off.
#[tokio::test]
async fn mcp_endpoint_answers_at_an_allowed_host_under_a_token() {
    set_token_once();
    let addr = free_addr();
    let db = Database::in_memory().unwrap();
    std::thread::spawn(move || {
        let opts = dr_strange_web::ServeOptions {
            addr,
            allowed_hosts: vec!["lan.example.com".into()],
            ..Default::default()
        };
        dr_strange_web::serve(db, None, opts).unwrap();
    });
    wait_ready(addr).await;

    let post = |host: &'static str| {
        reqwest::Client::new()
            .post(format!("http://{addr}/mcp"))
            .header("host", host)
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(r#"{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}},"id":1}"#)
            .send()
    };
    let allowed = post("lan.example.com").await.unwrap();
    assert_ne!(
        allowed.status(),
        reqwest::StatusCode::FORBIDDEN,
        "a listed host is answered"
    );
    let refused = post("other.example.com").await.unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::FORBIDDEN);
}

/// No `Authorization` header at all: refused before the tool router ever
/// sees the request, same posture as every other authenticated route on this
/// server. Tested over raw HTTP (like `auth_gate_over_http` in `http.rs`) —
/// this is about the axum middleware, not the MCP client.
#[tokio::test]
async fn mcp_endpoint_refuses_requests_with_no_token() {
    let addr = spawn_server();
    wait_ready(addr).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","method":"initialize","params":{},"id":1}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// An oversized body is refused on `/mcp`, the same as on `/rpc`.
///
/// Regression: `DefaultBodyLimit` guards the rest of the server, but axum
/// implements it as a request extension that only extractors consult, and
/// `/mcp` is a raw tower service that buffers the body itself — so an
/// authenticated caller could POST unbounded bytes into the process holding
/// the database.
#[tokio::test]
async fn mcp_endpoint_refuses_an_oversized_body() {
    let addr = spawn_server();
    wait_ready(addr).await;

    // Only the request head is written. The limit is enforced from the
    // declared `Content-Length`, so the server answers before a single byte
    // of the body is buffered — which is the property worth pinning, and the
    // reason this is raw TCP rather than `reqwest`: uploading the bytes for
    // real proves the same thing, but the server's early close then reaches
    // the client as a broken pipe instead of the 413 it already wrote.
    let too_big = 64 * 1024 * 1024 + 1;
    let head = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n\
         Content-Length: {too_big}\r\n\r\n"
    );
    let status = tokio::time::timeout(Duration::from_secs(30), async {
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        sock.write_all(head.as_bytes()).await.unwrap();
        let mut buf = [0u8; 64];
        let n = sock.read(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    })
    .await
    .expect("no response in 30s — the body was awaited, not refused");
    assert!(
        status.starts_with("HTTP/1.1 413"),
        "expected 413, got: {status}"
    );
}

/// Two independent MCP sessions sharing one `/mcp` endpoint see one
/// `Database`: session A's write is visible to session B, which never opened
/// the file and never talked to session A — only to the one server process.
/// This is ROADMAP §10's whole point, exercised over the real transport an
/// agent host uses.
#[tokio::test]
async fn two_sessions_share_one_database_over_mcp() {
    let addr = spawn_server();
    wait_ready(addr).await;

    // Session A: the seeded node round-trips.
    let a = connect(addr, TOKEN).await.unwrap();
    let alice = call(&a, "get_node", json!({"key": "alice"})).await;
    assert_eq!(alice["external_key"], "alice");

    // Session A writes a new node.
    let created = call(
        &a,
        "write_nodes",
        json!({"nodes": [{"external_key": "bob", "labels": ["Person"]}]}),
    )
    .await;
    assert_eq!(created["created"].as_array().unwrap().len(), 1);

    // Session B: a wholly separate connection (a different simulated agent
    // host) reads it back — the one thing a single-session test can't prove.
    let b = connect(addr, TOKEN).await.unwrap();
    let bob = call(&b, "get_node", json!({"key": "bob"})).await;
    assert_eq!(bob["external_key"], "bob");

    // A third session sees both writes reflected in the plane's count.
    let c = connect(addr, TOKEN).await.unwrap();
    let planes = call(&c, "list_planes", Value::Null).await;
    let startup = planes
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "startup")
        .unwrap();
    assert_eq!(startup["nodes"], 2);

    let _ = a.cancel().await;
    let _ = b.cancel().await;
    let _ = c.cancel().await;
}

/// The page a local browser loads carries the token, and carries it as data,
/// not code: a `<meta>` element the SPA reads, never an inline `<script>`,
/// so the response's `script-src 'self'` policy holds for the page itself.
/// This server is loopback-bound and the client is a loopback peer — the one
/// configuration in which the token is handed out at all (see `bind.rs` for
/// the other).
#[tokio::test]
async fn the_page_hands_its_token_to_a_loopback_browser_as_data_not_code() {
    let addr = spawn_server();
    wait_ready(addr).await;

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let csp = resp.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("script-src 'self'"), "{csp}");
    let html = resp.text().await.unwrap();
    assert!(
        html.contains(&format!(r#"<meta name="drsg-token" content="{TOKEN}">"#)),
        "{html}"
    );
    assert!(!html.contains("__DRSG_TOKEN__"), "{html}");
}

/// The same loopback listener hands no token to a request that a reverse
/// proxy on this machine forwarded: the peer is loopback either way, so what
/// tells them apart is what the proxy adds (`X-Forwarded-For`) or the public
/// name the browser addressed (`Host`). Each alone is enough, and the page
/// is still served — the SPA asks for the token instead.
#[tokio::test]
async fn the_page_hands_no_token_to_a_forwarded_or_publicly_addressed_request() {
    let addr = spawn_server();
    wait_ready(addr).await;
    let client = reqwest::Client::new();
    let meta = r#"<meta name="drsg-token""#;

    let forwarded = client
        .get(format!("http://{addr}/"))
        .header("x-forwarded-for", "203.0.113.9")
        .send()
        .await
        .unwrap();
    assert!(forwarded.status().is_success());
    let html = forwarded.text().await.unwrap();
    assert!(!html.contains(meta), "{html}");
    assert!(!html.contains(TOKEN), "{html}");

    let public = client
        .get(format!("http://{addr}/"))
        .header("host", "graph.example.com")
        .send()
        .await
        .unwrap();
    assert!(public.status().is_success());
    let html = public.text().await.unwrap();
    assert!(!html.contains(meta), "{html}");
    assert!(!html.contains(TOKEN), "{html}");

    // And the SPA route fallback follows the same rule.
    let deep = client
        .get(format!("http://{addr}/explore"))
        .header("via", "1.1 caddy")
        .send()
        .await
        .unwrap();
    assert!(!deep.text().await.unwrap().contains(meta));
}

/// `/ws` takes the bearer token either way: `Authorization: Bearer` on the
/// upgrade (the preferred form — a header is not written to access logs or
/// browser history) or `?token=` (the only form a browser's WebSocket API
/// can send). With neither, there is no socket at all.
#[tokio::test]
async fn the_websocket_takes_the_token_in_a_header_or_the_query() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let addr = spawn_server();
    wait_ready(addr).await;

    // Header.
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let first = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(first.contains("db.stats"), "{first}");

    // Query string.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?token={TOKEN}"))
        .await
        .unwrap();
    let first = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(first.contains("db.stats"), "{first}");

    // Neither: the upgrade is refused with 401.
    match tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status().as_u16(), 401);
        }
        Err(other) => panic!("expected an HTTP 401, got {other:?}"),
        Ok(_) => panic!("no credential, no socket"),
    }
}
