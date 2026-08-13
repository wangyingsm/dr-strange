//! The sandbox's sparring partner: one component, its behaviour picked by the
//! `options` each test passes, so five hostile behaviours need one artifact.
//!
//! Modes: `ok` (a node per file), `escape` (reads outside the root and
//! reports what the host said), `spin` (never terminates — fuel's job),
//! `alloc` (allocates without bound — the memory limit's job), `clock` (reads
//! the monotonic clock twice and reports the delta — zero when frozen).

wit_bindgen::generate!({
    path: "../../../wit",
    world: "plugin",
});

use exports::drsg::preprocess::preprocessor::{Guest, Input, Manifest, Node, Output, Report};

struct Fixture;

fn mode(options: &[(String, String)]) -> String {
    options
        .iter()
        .find(|(k, _)| k == "mode")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "ok".into())
}

fn node(key: &str, label: &str, props: &str) -> Node {
    Node {
        key: key.into(),
        label: label.into(),
        extra_labels: Vec::new(),
        properties: props.into(),
    }
}

fn out(nodes: Vec<Node>) -> Output {
    let facts = nodes.len() as u32;
    Output {
        nodes,
        edges: Vec::new(),
        prose: String::new(),
        report: Report {
            facts,
            prose_chars: 0,
            skipped: 0,
            notes: Vec::new(),
        },
    }
}

impl Guest for Fixture {
    fn describe() -> Manifest {
        Manifest {
            name: "fixture".into(),
            version: "0".into(),
            extensions: vec!["fix".into()],
        }
    }

    fn parse(subject: Input, options: Vec<(String, String)>) -> Result<Vec<u8>, String> {
        match mode(&options).as_str() {
            "ok" => {
                let names = match subject {
                    Input::Files(paths) => paths,
                    Input::Document(doc) => vec![doc.name],
                };
                Ok(names.join("\n").into_bytes())
            }
            "escape" => {
                // The interesting result is the host's refusal, verbatim.
                match drsg::preprocess::host::read("../../../etc/passwd") {
                    Ok(_) => Err("the host answered for a path outside its root".into()),
                    Err(why) => Ok(format!("refused: {why}").into_bytes()),
                }
            }
            "spin" => {
                let mut n = 0u64;
                loop {
                    n = n.wrapping_add(1);
                    std::hint::black_box(n);
                }
            }
            "alloc" => {
                let mut hoard: Vec<Vec<u8>> = Vec::new();
                loop {
                    hoard.push(vec![0u8; 1 << 20]);
                    std::hint::black_box(hoard.len());
                }
            }
            "clock" => {
                let a = std::time::Instant::now();
                let mut n = 0u64;
                for _ in 0..1000 {
                    n = n.wrapping_add(std::hint::black_box(1));
                }
                std::hint::black_box(n);
                let delta = a.elapsed().as_nanos();
                Ok(delta.to_string().into_bytes())
            }
            other => Err(format!("fixture has no mode `{other}`")),
        }
    }

    fn assemble(partials: Vec<Vec<u8>>, options: Vec<(String, String)>) -> Result<Output, String> {
        let joined: Vec<String> = partials
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        match mode(&options).as_str() {
            "ok" => Ok(out(joined
                .join("\n")
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| node(l, "Fixed", "{}"))
                .collect())),
            _ => Ok(out(vec![node(
                "fixture::result",
                "Fixed",
                &serde_escape(&joined.join("\n")),
            )])),
        }
    }
}

/// `{"value": <text>}` with just enough escaping for the fixture's own output.
fn serde_escape(text: &str) -> String {
    let escaped: String = text
        .chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' => vec!['\\', 'n'],
            c => vec![c],
        })
        .collect();
    format!("{{\"value\": \"{escaped}\"}}")
}

export!(Fixture);
