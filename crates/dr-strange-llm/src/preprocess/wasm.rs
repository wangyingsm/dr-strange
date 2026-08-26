//! Running a plugin (ROADMAP §11, slice 2).
//!
//! A plugin is a WebAssembly **component** loaded from a file the operator
//! installed. It implements the same [`Preprocessor`] trait the built-in
//! handlers do, so the router, the grounding and the conflict rule above are
//! untouched by the fact that this one is sandboxed and that one is not.
//!
//! ## Two phases, chunked by the host
//!
//! The contract is `parse` + `assemble`: the host splits the routed paths into
//! **fixed-size chunks** and runs `parse` over them in parallel — one fresh
//! `Store` each, sharing nothing — then calls `assemble` once with the partials
//! in chunk order. The chunk size is a constant rather than derived from the
//! core count, so the same tree chunks identically on every machine; the
//! parallelism lives out here where the cores are, and the guest stays
//! single-threaded.
//!
//! Partials are opaque bytes. The plugin serialises whatever its own
//! `assemble` wants to read back; this host shuttles them and never looks.
//! Cross-file resolution therefore stays in the plugin, deliberately — it is
//! language semantics, and the database holds none.
//!
//! ## A chunk that fails is a chunk, not the tree
//!
//! A chunk is one file, so a `parse` that traps is one file the plugin cannot
//! get through — **counted and skipped**, the way the built-in reader counts a
//! PNG it cannot convert. Real trees contain the file that breaks a parser:
//! generated source whose thousand-term expression recurses a walker past the
//! stack the guest was linked with, and no host setting moves that stack. One
//! such file used to refuse the whole repository, and under `serve watch` it
//! refused every fold after it too — the watcher stopped, and the graph stayed
//! empty while the server went on answering.
//!
//! Every chunk failing is the other thing entirely: a plugin that does not
//! work here, and ingesting nothing quietly would be the worst answer
//! available. That stays fatal, and `assemble` — one call, the whole tree —
//! has nothing to skip and always did.
//!
//! ## What a plugin can reach
//!
//! Exactly `list`, `read` and `label` — the [`Host`] trait, handed across the
//! boundary. No filesystem interface, no sockets, no environment, and **no way
//! to write anywhere**: whatever a plugin produces comes back as a return
//! value, and the host is what writes to the database.
//!
//! The grant is what the context holds, and it holds nothing: no preopened
//! directory, no network, no environment, no arguments. A guest runtime may
//! *import* `wasi:filesystem` — TinyGo's does before the plugin's first line
//! runs, and the Python and JS runtimes will too — but the preopen table is
//! empty, so there is no directory handle to read, probe, or enumerate.
//! `wasi:sockets` alone is refused at load by name: no runtime needs sockets
//! to start, so that import is intent rather than a startup shim.
//!
//! ## Bounded, and deterministic
//!
//! **Fuel**, not epoch interruption: fuel counts instructions, so a runaway
//! plugin stops at the same point on every machine, where epochs are
//! wall-clock and would tie the outcome to machine load. Memory is bounded per
//! store. Both are operator-settable; and no memory setting can lift the
//! 4 GiB ceiling wasm32 itself imposes — a tree whose facts exceed that is
//! ingested a subtree at a time, with the plane as the accumulator.
//!
//! Determinism is a matter of what the sandbox will answer: the clocks are
//! frozen and `wasi:random` deals a fixed byte sequence, so a runtime that
//! seeds hash or map order from entropy — Go seeds map iteration this way —
//! orders identically on every run, and re-ingesting a tree yields the same
//! graph.

use anyhow::{Context, Result, anyhow, bail};
use rayon::prelude::*;
use std::path::Path;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use super::{Host, Input, Manifest, PreprocessReport, Preprocessed, Preprocessor};
use crate::digest::{DigestEdge, DigestNode};

wasmtime::component::bindgen!({
    path: "wit",
    world: "plugin",
});

use drsg::preprocess::host as wit_host;
use exports::drsg::preprocess::preprocessor as wit_plugin;

/// wasmtime 47 carries its own anyhow-like error type that does not implement
/// `std::error::Error`, so anyhow's `.context` cannot chain it — bridge once,
/// keeping the cause chain `{:#}` renders.
fn wt(e: wasmtime::Error) -> anyhow::Error {
    anyhow!("{e:#}")
}

/// Paths per `parse` call: one, measured. The constant divides the tree into
/// the units the host can run in parallel, so it bounds parallelism from
/// above — at 64 this workspace made two chunks and ran two-wide on a machine
/// with far more cores. One file per call is the same per-file discipline the
/// native parser's rayon used, and it beat 8 and 64 on the bench; what it
/// multiplies is only a store and a pre-linked instantiation, which is why
/// pre-linking made it affordable. Kept a constant rather than derived from
/// the core count so the same tree chunks identically on every machine —
/// with one file a chunk, that guarantee is trivially true.
const CHUNK_FILES: usize = 1;

/// How much a plugin may spend, and how much it may hold.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Instructions any single call may execute. `None` disables the check,
    /// for a plugin the operator trusts on a tree big enough to make the
    /// ceiling a nuisance rather than a safeguard.
    pub fuel: Option<u64>,
    /// Linear memory per store, in bytes.
    pub memory_bytes: usize,
}

/// Sized from slice 1's measurement (~0.3–0.6 G instructions per MiB of Rust,
/// before wasm overhead), this carries on the order of half a gigabyte of
/// source through `assemble` — past `rust-lang/rust`. It is a net for the
/// plugin that does not terminate, not a budget for honest work; per-`parse`
/// calls see one small chunk and sit far below it.
const DEFAULT_FUEL: u64 = 200_000_000_000;

/// Three gigabytes — deliberately short of the 4 GiB ceiling wasm32 imposes
/// whatever this is set to.
const DEFAULT_MEMORY: usize = 3 << 30;

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: Some(DEFAULT_FUEL),
            memory_bytes: DEFAULT_MEMORY,
        }
    }
}

/// Interfaces whose mere presence in a component is a refusal.
///
/// Sockets only, and deliberately no longer `wasi:filesystem`. Refusing the
/// filesystem *import* by name was the first policy, and the TinyGo runtime
/// broke it honestly: a Go guest imports `wasi:filesystem/preopens` before
/// its first line runs, as the Python and JS runtimes will too — the import
/// is the toolchain's startup shim, not the plugin's intent. What holds
/// instead is the grant itself: the context below preopens **nothing**, so
/// there is no directory handle to read, probe, or enumerate — every open
/// fails on an empty table. Sockets stay refused by name because no
/// runtime needs them to start, so that import *is* intent.
const FORBIDDEN: &[&str] = &["wasi:sockets"];

/// The bytes `wasi:random` deals, forever. Any constant would do; this one
/// spells the project, which makes it recognisable in a debugger.
const FIXED_ENTROPY: [u8; 4] = *b"drsg";

/// A plugin, compiled once and ready to run.
///
/// The `Engine` owns the compiled code and the `Component` shares it across
/// every call; only `Store`s — cheap, and holding all the mutable state — are
/// made per call.
pub struct WasmPlugin {
    engine: Engine,
    /// The component, already linked: WASI and the host interface are wired
    /// up **once here**, so a call pays for a store and an instantiation, not
    /// for rebuilding the linker — measured, that rebuild was a large share
    /// of the per-call cost.
    instance_pre: PluginPre<State>,
    manifest: Manifest,
    limits: Limits,
    /// This plugin's own settings, from `[plugins.<name>]`. Passed through
    /// uninterpreted: what a plugin can be configured to do is the plugin's
    /// business, not the database's.
    options: Vec<(String, String)>,
}

impl WasmPlugin {
    /// Compile a component and ask it what it is.
    ///
    /// `describe()` is asked of the component itself rather than trusted from
    /// the registry, so a record that drifted from the artifact is caught here.
    pub fn load(path: &Path, options: Vec<(String, String)>, limits: Limits) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("reading plugin {}", path.display()))?;
        Self::from_bytes(&bytes, options, limits)
            .with_context(|| format!("loading plugin {}", path.display()))
    }

    pub fn from_bytes(
        bytes: &[u8],
        options: Vec<(String, String)>,
        limits: Limits,
    ) -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        // Fuel is why a runaway plugin is *interrupted* rather than left to
        // spin, and it is deterministic in a way epoch interruption is not.
        config.consume_fuel(limits.fuel.is_some());

        let engine = Engine::new(&config)
            .map_err(wt)
            .context("starting the wasm engine")?;
        let component = Component::new(&engine, bytes).map_err(wt).context(
            "this file is not a WebAssembly component — a plugin must be a \
             component, not a core module",
        )?;

        refuse_forbidden_imports(&engine, &component)?;

        // Link once. Instantiation from a pre-linked component is cheap; the
        // linker construction it replaces registered every WASI function on
        // every call.
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(wt)
            .context("linking the wasi interfaces a guest needs to start")?;
        Plugin::add_to_linker::<_, wasmtime::component::HasSelf<_>>(&mut linker, |s| s)
            .map_err(wt)
            .context("linking the host interface")?;
        let instance_pre = PluginPre::new(
            linker
                .instantiate_pre(&component)
                .map_err(wt)
                .context("pre-instantiating the plugin")?,
        )
        .map_err(wt)
        .context("the component does not export the plugin world")?;

        let mut plugin = Self {
            engine,
            instance_pre,
            manifest: Manifest {
                name: "<undescribed>".into(),
                version: String::new(),
                extensions: Vec::new(),
                logo: None,
            },
            limits,
            options,
        };
        plugin.manifest = plugin.ask_describe()?;
        Ok(plugin)
    }

    /// What the component says it is, normalised so routing compares like with
    /// like whatever the author happened to write.
    fn ask_describe(&self) -> Result<Manifest> {
        self.with_instance(None, |instance, store| {
            let m = instance
                .drsg_preprocess_preprocessor()
                .call_describe(store)
                .map_err(wt)
                .context("asking the plugin to describe itself")?;
            if m.name.trim().is_empty() {
                bail!("this plugin describes itself with an empty name");
            }
            Ok(Manifest {
                name: m.name.trim().to_ascii_lowercase(),
                version: m.version,
                extensions: m
                    .extensions
                    .into_iter()
                    .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|e| !e.is_empty())
                    .collect(),
                logo: m.logo.filter(|l| !l.trim().is_empty()),
            })
        })
    }

    /// One `parse` call: one chunk in, one opaque partial out.
    fn parse_chunk(&self, subject: wit_plugin::Input, host: &dyn Host) -> Result<Vec<u8>> {
        self.with_instance(Some(host), |instance, store| {
            instance
                .drsg_preprocess_preprocessor()
                .call_parse(&mut *store, &subject, &self.options)
                .map_err(|e| self.explain_trap(e, store))?
                .map_err(|why| anyhow!("plugin `{}` failed to parse: {why}", self.manifest.name))
        })
    }

    /// The one `assemble` call, with partials in chunk order.
    fn assemble(&self, partials: &[Vec<u8>], host: &dyn Host) -> Result<wit_plugin::Output> {
        self.with_instance(Some(host), |instance, store| {
            instance
                .drsg_preprocess_preprocessor()
                .call_assemble(&mut *store, partials, &self.options)
                .map_err(|e| self.explain_trap(e, store))?
                .map_err(|why| anyhow!("plugin `{}` failed to assemble: {why}", self.manifest.name))
        })
    }

    /// A fresh store, a fresh instance, one call, and both are gone.
    fn with_instance<R>(
        &self,
        host: Option<&dyn Host>,
        call: impl FnOnce(&Plugin, &mut Store<State>) -> Result<R>,
    ) -> Result<R> {
        // SAFETY: `Store<T>` demands `T: 'static`, and this reference is not.
        // The store is created below, used for exactly one call, and dropped
        // when this function returns; the state is never cloned or moved out
        // of it. So the reference cannot outlive the borrow it widens — the
        // `'static` exists for the type system, not for the program.
        let host: Option<&'static dyn Host> =
            host.map(|h| unsafe { std::mem::transmute::<&dyn Host, &'static dyn Host>(h) });
        // Nothing is granted: no preopened directory, no network, no
        // environment, no arguments. File access exists only through the
        // `drsg:preprocess/host` interface below, which is rooted and checked.
        //
        // The clocks are **frozen**, not merely untrusted: §11's determinism
        // promise is that re-ingesting a tree yields the same graph, and a
        // plugin that could read a real clock could fold time into its facts.
        // A frozen clock makes that impossible rather than impolite.
        // Entropy is **fixed**, not merely untrusted, for the same reason the
        // clocks are frozen: a Go runtime seeds its map iteration order from
        // `wasi:random`, so real entropy would make the same tree emit facts
        // in a different order on every run. Every store deals the same
        // bytes, so every run is the same run.
        // Stderr is captured, not discarded: a Go runtime prints its panic
        // there and then traps, and the message is the whole diagnosis. The
        // capacity is a guard, not a budget — a plugin writing a megabyte of
        // stderr in one call has something wrong worth stopping over.
        let stderr = wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(1 << 20);
        let wasi = WasiCtxBuilder::new()
            .stderr(stderr.clone())
            .monotonic_clock(FrozenInstant)
            .wall_clock(FrozenWall)
            .secure_random(wasmtime_wasi::Deterministic::new(FIXED_ENTROPY.to_vec()))
            .insecure_random(wasmtime_wasi::Deterministic::new(FIXED_ENTROPY.to_vec()))
            .insecure_random_seed(0)
            .build();
        let mut store = Store::new(
            &self.engine,
            State {
                host,
                wasi,
                table: ResourceTable::new(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(self.limits.memory_bytes)
                    .build(),
                stderr,
            },
        );
        store.limiter(|s| &mut s.limits);
        if let Some(fuel) = self.limits.fuel {
            // Cannot fail: the engine was configured to consume fuel above.
            let _ = store.set_fuel(fuel);
        }

        let instance = self
            .instance_pre
            .instantiate(&mut store)
            .map_err(wt)
            .context("instantiating the plugin")?;

        call(&instance, &mut store)
    }

    /// Turn a trap into something a reader can act on.
    ///
    /// Running out of fuel is the one an operator can do something about, so
    /// it says so and names the setting, rather than surfacing as "wasm trap".
    ///
    /// Everything else is rendered `{:#}`, not `{}`, and **led by the trap
    /// code**: wasmtime puts the wasm backtrace in the outer message and the
    /// code that names what went wrong in the cause, so the plain form opens
    /// with "error while executing at wasm backtrace: …", twenty frames of
    /// recursion, and never says why. A guest that blew its own stack is
    /// indistinguishable from one that divided by zero until the last line —
    /// which is exactly the line a log line, truncated, drops.
    fn explain_trap(&self, error: wasmtime::Error, store: &Store<State>) -> anyhow::Error {
        let name = &self.manifest.name;
        if self.limits.fuel.is_some() && store.get_fuel().is_ok_and(|left| left == 0) {
            return anyhow!(
                "plugin `{name}` ran out of fuel — either it does not terminate, \
                 or the input is larger than `[plugins] fuel` allows for"
            );
        }
        let rendered = format!("{error:#}");
        let code = trap_code(&rendered)
            .map(|c| format!(": {c}"))
            .unwrap_or_default();
        let hint = stack_overflow_hint(&rendered)
            .map(|h| format!(" — {h}"))
            .unwrap_or_default();
        let said = store.data().stderr.contents();
        if said.is_empty() {
            return anyhow!("plugin `{name}` trapped{code}{hint}\n{rendered}");
        }
        // The tail, because a panic message comes last and the front of a
        // long log is the least interesting part of it.
        let tail = said.len().saturating_sub(2048);
        let said = String::from_utf8_lossy(&said[tail..]).trim().to_string();
        anyhow!("plugin `{name}` trapped{code}{hint}\n{rendered}\nits stderr said:\n{said}")
    }
}

impl Preprocessor for WasmPlugin {
    fn manifest(&self) -> Manifest {
        self.manifest.clone()
    }

    fn preprocess(&self, input: &Input<'_>, host: &dyn Host) -> Result<Preprocessed> {
        let started = std::time::Instant::now();
        // Phase one: chunks in parallel, partials collected in chunk order —
        // par_iter's collect preserves order, which is the same discipline the
        // native parser used for exactly the same reason.
        let mut refused: Vec<String> = Vec::new();
        let mut first_refusal: Option<anyhow::Error> = None;
        let partials: Vec<Vec<u8>> = match input {
            Input::Document { name, bytes } => {
                vec![self.parse_chunk(
                    wit_plugin::Input::Document(wit_plugin::Doc {
                        name: (*name).to_string(),
                        bytes: bytes.to_vec(),
                    }),
                    host,
                )?]
            }
            Input::Files { paths } => {
                let outcomes: Vec<(&[String], Result<Vec<u8>>)> = paths
                    .par_chunks(CHUNK_FILES)
                    .map(|chunk| {
                        let out = self.parse_chunk(wit_plugin::Input::Files(chunk.to_vec()), host);
                        (chunk, out)
                    })
                    .collect();
                let mut kept = Vec::with_capacity(outcomes.len());
                for (chunk, outcome) in outcomes {
                    match outcome {
                        Ok(partial) => kept.push(partial),
                        // A file the plugin cannot get through is **counted,
                        // not fatal** — the same rule the built-in reader
                        // applies to a PNG it cannot convert. One pathological
                        // file (generated source that recurses a walker past
                        // its stack, say) would otherwise refuse a whole
                        // repository, and under `serve watch` it refuses every
                        // fold from then on: the watcher stops and the graph
                        // silently stays empty.
                        Err(e) => {
                            tracing::warn!(
                                plugin = %self.manifest.name,
                                files = %chunk.join(", "),
                                error = format!("{e:#}"),
                                "the plugin could not parse these files — skipped"
                            );
                            refused.extend(chunk.iter().cloned());
                            first_refusal.get_or_insert(e);
                        }
                    }
                }
                // Failing on *everything*, though, is not a pathological file:
                // it is a plugin that does not work here, and quietly ingesting
                // nothing would be the worst answer available.
                if kept.is_empty() && !refused.is_empty() {
                    let n = refused.len();
                    return Err(first_refusal
                        .expect("a refusal was recorded")
                        .context(format!(
                            "plugin `{}` failed on all {n} of the files it claimed",
                            self.manifest.name
                        )));
                }
                kept
            }
        };
        let parsed = std::time::Instant::now();

        // Phase two: once, with everything that got through.
        let out = self.assemble(&partials, host)?;
        // Where a slow run spends its time: the parse phase is as wide as the
        // chunk count, the assemble phase is serial by design — knowing which
        // one dominates decides where tuning is worth anything.
        tracing::debug!(
            plugin = %self.manifest.name,
            chunks = partials.len(),
            parse_ms = parsed.duration_since(started).as_millis() as u64,
            assemble_ms = parsed.elapsed().as_millis() as u64,
            "preprocess phases"
        );
        let mut pre = into_preprocessed(out, &self.manifest.name)?;
        if !refused.is_empty() {
            pre.report.skipped += refused.len();
            let why = first_refusal.map(|e| format!("{e:#}")).unwrap_or_default();
            pre.report
                .notes
                .push(skipped_note(&self.manifest.name, &refused, &why));
        }
        Ok(pre)
    }
}

/// The trap code out of a rendered error chain — `wasm trap: out of bounds
/// memory access` and the like, which wasmtime writes as the innermost cause
/// and this lifts to the front so the first line of the message is the one
/// that says what happened.
fn trap_code(rendered: &str) -> Option<&str> {
    const MARK: &str = "wasm trap: ";
    let at = rendered.rfind(MARK)? + MARK.len();
    Some(rendered[at..].lines().next()?.trim()).filter(|c| !c.is_empty())
}

/// Does this trap read like a guest that ran out of stack?
///
/// Three shapes say so. Wasmtime's own `call stack exhausted` is the plain
/// one. A guest whose stack lives at the bottom of linear memory and grows
/// *down* — TinyGo's does, and wasi-libc's can — instead faults at an address
/// just below 4 GiB: a stack pointer that decremented past zero and wrapped,
/// not an index into anything. And where the fault carries no address, the
/// backtrace still does: the same frame, all the way down.
///
/// Worth naming, because the cure is not the operator's to apply: a guest's
/// stack is sized into the module when it is linked, and no host setting
/// moves it. What moves it is the plugin — or the input, since what reaches
/// that depth is usually generated source (a thousand-term `+` chain in a
/// protobuf blob, say) walked by a hand-written recursive printer.
fn stack_overflow_hint(rendered: &str) -> Option<&'static str> {
    const HINT: &str = "the guest overflowed its own stack, which deeply nested input does \
         to a recursive walker. A plugin's stack is fixed when it is linked, \
         so no host setting raises it: this one is for the plugin's author";
    if rendered.contains("call stack exhausted") {
        return Some(HINT);
    }
    if !rendered.contains("out of bounds memory access") {
        return None;
    }
    (wrapped_past_zero(rendered) || one_frame_all_the_way_down(rendered)).then_some(HINT)
}

/// A fault address within a page of the wasm32 ceiling: nothing is addressed
/// up there, so it is a pointer that went negative.
fn wrapped_past_zero(rendered: &str) -> bool {
    let Some(rest) = rendered.split("memory fault at wasm address ").nth(1) else {
        return false;
    };
    let hex = rest
        .split(|c: char| !c.is_ascii_hexdigit() && c != 'x')
        .next();
    let addr = hex
        .and_then(|h| h.strip_prefix("0x"))
        .and_then(|h| u64::from_str_radix(h, 16).ok());
    addr.is_some_and(|a| a >= u64::from(u32::MAX) - (1 << 16))
}

/// Runaway recursion, read off the backtrace: wasmtime lists the innermost
/// frames, and when nearly all of them are the same function the guest was
/// descending, not working.
fn one_frame_all_the_way_down(rendered: &str) -> bool {
    // `    3:  0x4f628 - main!go/printer.walkBinary` — and the deepest line may
    // carry the trap text after a colon, which is not part of the name.
    let frames: Vec<&str> = rendered
        .lines()
        .filter(|l| l.contains(" - ") && l.contains("0x"))
        .filter_map(|l| l.split(" - ").nth(1))
        .map(|f| f.split(':').next().unwrap_or(f).trim())
        .collect();
    // The most repeated name, not the innermost one: a recursive walker often
    // trips inside a small helper it calls. Twenty frames at most, so counting
    // them against each other costs nothing worth a hash map.
    let deepest_repeat = frames
        .iter()
        .map(|a| frames.iter().filter(|b| *b == a).count())
        .max()
        .unwrap_or(0);
    frames.len() >= 8 && deepest_repeat * 4 >= frames.len() * 3
}

/// The one line a report carries about files a plugin could not get through.
///
/// Aggregated rather than one note per file: a tree can hand a plugin
/// thousands of files, and a report is read by a person. The paths are the
/// actionable part, so a few of them are named outright and the rest counted;
/// the plugin's own words come last, first line only — the wasm backtrace
/// behind it is already in the log at `warn`.
fn skipped_note(plugin: &str, refused: &[String], why: &str) -> String {
    const NAMED: usize = 3;
    let mut which = refused.iter().take(NAMED).cloned().collect::<Vec<_>>();
    if let Some(rest) = refused.len().checked_sub(NAMED).filter(|r| *r > 0) {
        which.push(format!("and {rest} more"));
    }
    let why = why.lines().next().unwrap_or_default().trim();
    let n = refused.len();
    let (files, whose) = if n == 1 {
        ("file", "its facts are")
    } else {
        ("files", "their facts are")
    };
    format!(
        "plugin `{plugin}` could not parse {n} {files} — skipped, so {whose} \
         missing from this run: {}{}",
        which.join(", "),
        if why.is_empty() {
            String::new()
        } else {
            format!(" ({why})")
        }
    )
}

/// Time, stopped. Both wasi clocks return a constant, so two reads agree and
/// a run's output cannot depend on when it ran.
struct FrozenInstant;

impl wasmtime_wasi::HostMonotonicClock for FrozenInstant {
    fn resolution(&self) -> u64 {
        1
    }
    fn now(&self) -> u64 {
        0
    }
}

struct FrozenWall;

impl wasmtime_wasi::HostWallClock for FrozenWall {
    fn resolution(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(1)
    }
    fn now(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}

/// What the guest may reach, and the wasi plumbing it needs to start.
struct State {
    /// `None` while asking a component to describe itself — answering that
    /// question needs no files, so it is given none.
    ///
    /// `'static` is a lie the type system requires: `Store<T>` demands
    /// `T: 'static`, and the reference actually lives exactly as long as the
    /// call in [`WasmPlugin::with_instance`]. The invariant that makes the
    /// widening sound is stated there, where it is upheld.
    host: Option<&'static dyn Host>,
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    /// Where the guest's stderr lands. Discarding it kept a Go plugin's
    /// panic message invisible behind a bare "trapped" — so it is captured
    /// instead, and surfaced when a trap has to be explained.
    stderr: wasmtime_wasi::p2::pipe::MemoryOutputPipe,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl wit_host::Host for State {
    fn list(&mut self, suffix: String) -> Result<Vec<String>, String> {
        self.files()?.list(&suffix).map_err(|e| e.to_string())
    }

    fn read(&mut self, path: String) -> Result<Vec<u8>, String> {
        self.files()?.read(&path).map_err(|e| e.to_string())
    }

    fn label(&mut self) -> Option<String> {
        self.host.and_then(|h| h.label())
    }
}

impl State {
    fn files(&self) -> Result<&'static dyn Host, String> {
        self.host
            .ok_or_else(|| "this call was given no files to read".to_string())
    }
}

/// Refuse a component that asks for a capability a preprocessor cannot need.
fn refuse_forbidden_imports(engine: &Engine, component: &Component) -> Result<()> {
    let ty = component.component_type();
    for (name, _) in ty.imports(engine) {
        if let Some(bad) = FORBIDDEN.iter().find(|f| name.starts_with(**f)) {
            bail!(
                "this plugin imports `{name}`, and a preprocessor is not given \
                 {bad}: it reads through the host interface, which is rooted at \
                 the directory being ingested, and it never writes anywhere"
            );
        }
    }
    Ok(())
}

/// Cross back: WIT records into the shapes the digest pipeline already writes.
fn into_preprocessed(out: wit_plugin::Output, plugin: &str) -> Result<Preprocessed> {
    let nodes = out
        .nodes
        .into_iter()
        .map(|n| {
            Ok(DigestNode {
                key: n.key,
                label: n.label,
                extra_labels: n.extra_labels,
                props: properties(&n.properties, plugin)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let edges = out
        .edges
        .into_iter()
        .map(|e| {
            Ok(DigestEdge {
                src: e.src,
                dst: e.dst,
                ty: e.type_,
                props: properties(&e.properties, plugin)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Preprocessed {
        nodes,
        edges,
        prose: out.prose,
        report: PreprocessReport {
            // The router records who ran; a plugin does not name itself.
            handlers: Vec::new(),
            prose_chars: out.report.prose_chars as usize,
            skipped: out.report.skipped as usize,
            collisions: Vec::new(),
            notes: out.report.notes,
        },
    })
}

/// Properties travel as a JSON object, because a value may be a list or a map
/// of values and WIT has no recursive types. It is the same shape
/// `digest.write` accepts, so the conversion already existed.
fn properties(json: &str, plugin: &str) -> Result<dr_strange_core::Properties> {
    if json.trim().is_empty() || json.trim() == "{}" {
        return Ok(dr_strange_core::Properties::new());
    }
    let value: serde_json::Value = serde_json::from_str(json).with_context(|| {
        format!("plugin `{plugin}` returned properties that are not JSON: {json:.120}")
    })?;
    dr_strange_core::json::json_to_properties(&value)
        .map_err(|e| anyhow!("plugin `{plugin}` returned properties we cannot read: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The go plugin, on a generated `.pb.go` whose `rawDesc` is a
    /// thousand-term string concatenation — the trap this reading exists for,
    /// verbatim but for the frames elided in the middle.
    const GO_STACK_OVERFLOW: &str = "\
error while executing at wasm backtrace:
    0:  0x4f79b - main!go/printer.walkBinary
    1:  0x4f628 - main!go/printer.walkBinary
    2:  0x4f628 - main!go/printer.walkBinary
    3:  0x4f628 - main!go/printer.walkBinary
    4:  0x4f628 - main!go/printer.walkBinary
    5:  0x4f628 - main!go/printer.walkBinary
    6:  0x4f628 - main!go/printer.walkBinary
    7:  0x4f628 - main!go/printer.walkBinary
    8:  0x4f628 - main!go/printer.walkBinary
    9:  0x4cd01 - main!(*go/printer.printer).expr1: memory fault at wasm \
address 0xfffffffc in linear memory of size 0x40000: wasm trap: out of bounds \
memory access";

    #[test]
    fn the_trap_code_is_lifted_out_of_the_cause() {
        assert_eq!(
            trap_code(GO_STACK_OVERFLOW),
            Some("out of bounds memory access")
        );
        assert_eq!(trap_code("nothing wasm about this"), None);
    }

    #[test]
    fn a_wrapped_stack_pointer_reads_as_a_stack_overflow() {
        assert!(stack_overflow_hint(GO_STACK_OVERFLOW).is_some());
        assert!(wrapped_past_zero(GO_STACK_OVERFLOW));
        assert!(stack_overflow_hint("wasm trap: call stack exhausted").is_some());
    }

    /// A fault with no address at all still reads as recursion when the
    /// backtrace is one frame repeated — which is what a Rust guest gives.
    #[test]
    fn one_frame_repeated_reads_as_recursion() {
        let mut trace = String::from("error while executing at wasm backtrace:\n");
        for i in 0..12 {
            trace.push_str(&format!(
                "    {i}:   0x45f2 - fixture.wasm!walk_off_the_stack\n"
            ));
        }
        trace.push_str("wasm trap: out of bounds memory access");
        assert!(stack_overflow_hint(&trace).is_some());
    }

    /// An ordinary bug is not dressed up as a stack overflow: a short, varied
    /// backtrace gets the trap code and nothing more.
    #[test]
    fn an_ordinary_trap_gets_no_stack_reading() {
        let trace = "\
error while executing at wasm backtrace:
    0:   0x120 - plugin.wasm!parse_header
    1:   0x340 - plugin.wasm!parse
    2:   0x510 - plugin.wasm!main: wasm trap: integer divide by zero";
        assert_eq!(trap_code(trace), Some("integer divide by zero"));
        assert_eq!(stack_overflow_hint(trace), None);
    }

    #[test]
    fn the_skip_note_names_a_few_files_and_counts_the_rest() {
        let refused: Vec<String> = (0..5).map(|i| format!("gen/{i}.pb.go")).collect();
        let note = skipped_note(
            "go",
            &refused,
            "plugin `go` trapped: out of bounds\nframe 0",
        );
        assert!(note.contains("could not parse 5 files"), "{note}");
        assert!(
            note.contains("gen/0.pb.go, gen/1.pb.go, gen/2.pb.go"),
            "{note}"
        );
        assert!(note.contains("and 2 more"), "{note}");
        // The first line of the plugin's complaint, and not the backtrace
        // under it.
        assert!(note.contains("out of bounds"), "{note}");
        assert!(!note.contains("frame 0"), "{note}");

        let one = skipped_note("go", &["a.go".to_string()], "");
        assert!(one.contains("1 file — skipped, so its facts are"), "{one}");
        assert!(!one.contains("more"), "{one}");
    }
}
