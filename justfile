# dr-strange task runner. `just --list` to see recipes.

# Build the web dashboard SPA (bun + Vite). Its output in
# crates/dr-strange-web/frontend/dist is embedded into the drsg binary at
# compile time, so run this before `cargo build` to ship the real UI (a
# placeholder page is embedded otherwise — see crates/dr-strange-web/build.rs).
web-build:
    cd crates/dr-strange-web/frontend && bun install && bun run build

# Run the SPA dev server (hot reload); proxies /rpc and /ws to a locally
# running `drsg serve` on port 7700.
web-dev:
    cd crates/dr-strange-web/frontend && bun install && bun run dev

# Serve a database with the dashboard + JSON-RPC API (default graph.drsg).
serve db="graph.drsg":
    cargo run -p dr-strange-cli -- --db {{db}} serve
