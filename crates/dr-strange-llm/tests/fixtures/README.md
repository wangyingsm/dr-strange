# Sandbox fixtures

Committed artifacts, so the host's tests run without a wasm toolchain. Rebuild
when `crates/dr-strange-llm/wit/preprocess.wit` changes:

```
cargo build --manifest-path crates/dr-strange-llm/tests/fixtures/guest/Cargo.toml \
    --target wasm32-wasip2 --release
cargo build --manifest-path crates/dr-strange-llm/tests/fixtures/guest-fs/Cargo.toml \
    --target wasm32-wasip2 --release
cp crates/dr-strange-llm/tests/fixtures/guest/target/wasm32-wasip2/release/drsg_fixture.wasm \
   crates/dr-strange-llm/tests/fixtures/fixture.wasm
cp crates/dr-strange-llm/tests/fixtures/guest-fs/target/wasm32-wasip2/release/drsg_fixture_fs.wasm \
   crates/dr-strange-llm/tests/fixtures/fixture-fs.wasm
```

- `fixture.wasm` — one component, its behaviour picked by `options`:
  `ok`, `escape`, `spin`, `alloc`, `clock`. See `guest/src/lib.rs`.
- `fixture-fs.wasm` — imports `wasi:filesystem` (via `std::fs`), and exists to
  be refused by name at load.
