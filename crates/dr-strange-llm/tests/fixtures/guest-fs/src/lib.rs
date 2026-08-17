//! A component whose only purpose is to import `wasi:filesystem` — which
//! `std::fs` forces on wasm32-wasip2 — so the host's refusal-by-import-name
//! has something real to refuse.

wit_bindgen::generate!({
    path: "../../../wit",
    world: "plugin",
});

use exports::drsg::preprocess::preprocessor::{Guest, Input, Manifest, Output, Report};

struct Grabby;

impl Guest for Grabby {
    fn describe() -> Manifest {
        Manifest {
            name: "grabby".into(),
            version: "0".into(),
            extensions: vec!["grab".into()],
            logo: None,
        }
    }

    fn parse(_subject: Input, _options: Vec<(String, String)>) -> Result<Vec<u8>, String> {
        // The call is what plants the import; it never gets to run.
        std::fs::read("/etc/passwd").map_err(|e| e.to_string())
    }

    fn assemble(_partials: Vec<Vec<u8>>, _options: Vec<(String, String)>) -> Result<Output, String> {
        Ok(Output {
            nodes: Vec::new(),
            edges: Vec::new(),
            prose: String::new(),
            report: Report {
                facts: 0,
                prose_chars: 0,
                skipped: 0,
                notes: Vec::new(),
            },
        })
    }
}

export!(Grabby);
