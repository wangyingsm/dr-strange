//! Issue #84: a proxy in the environment must not swallow the loopback server
//! `drsg init` declared.
//!
//! The binary, not the library: the defect was reqwest selecting a proxy from
//! the environment on its own, and an environment belongs to a process. Setting
//! one in-process would race every other test in the binary, so the variables
//! are handed to a child instead — which is also how the bug was reported.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A loopback listener that counts the connections it accepts and answers each
/// with a bare `200 OK`. Standing in for the declared server it is healthy;
/// standing in for the proxy it is a tripwire, and the count must stay at nil.
async fn counting_listener() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&hits);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            seen.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .await;
            let _ = socket.flush().await;
        }
    });
    (addr, hits)
}

#[tokio::test]
async fn a_proxy_in_the_environment_does_not_intercept_the_declared_server() {
    let (server, reached) = counting_listener().await;
    let (proxy, proxied) = counting_listener().await;

    // What `drsg init` writes, in a repository the binary discovers from.
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    let declaration = repo.path().join(".mcp.json");
    std::fs::write(
        &declaration,
        json!({
            "mcpServers": {
                "drsg-watch": {
                    "type": "http",
                    "url": format!("http://{server}/mcp"),
                    "headers": { "Authorization": "Bearer not-a-real-token" },
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    // `relay::trusted` reads nothing group- or world-writable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&declaration, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_drsg-mcp"))
        .current_dir(repo.path())
        .env("http_proxy", format!("http://{proxy}"))
        .env("https_proxy", format!("http://{proxy}"))
        .env("ALL_PROXY", format!("http://{proxy}"))
        .env_remove("no_proxy")
        .env_remove("NO_PROXY")
        .env_remove("DRSG_DB")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the binary runs");

    // Wait for the probe to land rather than for the process to exit: the
    // probe is the first thing it does, and what happens afterwards is not
    // this test's business. A regression spends the whole budget instead,
    // because the probe reaches the proxy and the server is never asked.
    let probed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while reached.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    let _ = child.start_kill();
    let _ = child.wait().await;

    assert!(
        probed.is_ok(),
        "the declared server must be probed directly, not through the proxy",
    );
    assert_eq!(
        proxied.load(Ordering::SeqCst),
        0,
        "nothing may be sent to the proxy",
    );
}
