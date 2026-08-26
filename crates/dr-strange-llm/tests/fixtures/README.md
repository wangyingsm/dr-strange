# Sandbox fixtures

Committed artifacts, so the host's tests run without a wasm toolchain. Rebuild
when `crates/dr-strange-llm/wit/preprocess.wit` changes:

```
cargo build --manifest-path crates/dr-strange-llm/tests/fixtures/guest/Cargo.toml \
    --target wasm32-wasip2 --release
cargo build --manifest-path crates/dr-strange-llm/tests/fixtures/guest-fs/Cargo.toml \
    --target wasm32-wasip2 --release
cargo build --manifest-path crates/dr-strange-llm/tests/fixtures/guest-net/Cargo.toml \
    --target wasm32-wasip2 --release
cp crates/dr-strange-llm/tests/fixtures/guest/target/wasm32-wasip2/release/drsg_fixture.wasm \
   crates/dr-strange-llm/tests/fixtures/fixture.wasm
cp crates/dr-strange-llm/tests/fixtures/guest-fs/target/wasm32-wasip2/release/drsg_fixture_fs.wasm \
   crates/dr-strange-llm/tests/fixtures/fixture-fs.wasm
cp crates/dr-strange-llm/tests/fixtures/guest-net/target/wasm32-wasip2/release/drsg_fixture_net.wasm \
   crates/dr-strange-llm/tests/fixtures/fixture-net.wasm
```

- `fixture.wasm` — one component, its behaviour picked by `options`:
  `ok`, `escape`, `spin`, `alloc`, `clock`, `rand`, `stack`. See
  `guest/src/lib.rs`. `stack` recurses off its own stack, but only on files
  named `deep…`, so the host's "skip that file, keep the tree" is testable
  against a mixed directory.
- `fixture-fs.wasm` — imports `wasi:filesystem` (via `std::fs`). It loads —
  guest runtimes plant this import before a plugin's first line runs — and
  exists to prove the grant behind it is an empty preopen table.
- `fixture-net.wasm` — imports `wasi:sockets` (via `std::net`), and exists to
  be refused by name at load: nothing needs sockets to start, so that import
  is intent.
