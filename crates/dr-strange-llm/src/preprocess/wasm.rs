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
//! ## What a plugin can reach
//!
//! Exactly `list`, `read` and `label` — the [`Host`] trait, handed across the
//! boundary. No filesystem interface, no sockets, no environment, and **no way
//! to write anywhere**: whatever a plugin produces comes back as a return
//! value, and the host is what writes to the database.
//!
//! The grant is enforced twice over. A component that imports
//! `wasi:filesystem` or `wasi:sockets` is refused at load — a preprocessor has
//! no business with either, and a loud refusal beats an empty implementation
//! it can probe at. What is linked is then only what a guest needs to start
//! (`wasi:cli`, `wasi:clocks`, `wasi:io`), with a context granting no
//! preopens, no network, no environment and no arguments.
//!
//! ## Bounded, and deterministic
//!
//! **Fuel**, not epoch interruption: fuel counts instructions, so a runaway
//! plugin stops at the same point on every machine, where epochs are
//! wall-clock and would tie the outcome to machine load. Memory is bounded per
//! store. Both are operator-settable; and no memory setting can lift the
//! 4 GiB ceiling wasm32 itself imposes — a tree whose facts exceed that is
//! ingested a subtree at a time, with the plane as the accumulator.

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
/// Not "linked but empty": a preprocessor has no business reaching a
/// filesystem or a socket, and a component asking for one is telling us what
/// it is. The error names the interface, so the refusal is legible rather
/// than an instantiation failure the operator has to decode.
const FORBIDDEN: &[&str] = &["wasi:filesystem", "wasi:sockets"];

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
        let wasi = WasiCtxBuilder::new()
            .monotonic_clock(FrozenInstant)
            .wall_clock(FrozenWall)
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
    fn explain_trap(&self, error: wasmtime::Error, store: &Store<State>) -> anyhow::Error {
        let name = &self.manifest.name;
        if self.limits.fuel.is_some() && store.get_fuel().is_ok_and(|left| left == 0) {
            return anyhow!(
                "plugin `{name}` ran out of fuel — either it does not terminate, \
                 or the input is larger than `[plugins] fuel` allows for"
            );
        }
        anyhow!("plugin `{name}` trapped: {error}")
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
            Input::Files { paths } => paths
                .chunks(CHUNK_FILES)
                .collect::<Vec<_>>()
                .par_iter()
                .map(|chunk| self.parse_chunk(wit_plugin::Input::Files(chunk.to_vec()), host))
                .collect::<Result<Vec<_>>>()?,
        };
        let parsed = std::time::Instant::now();

        // Phase two: once, with everything.
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
        into_preprocessed(out, &self.manifest.name)
    }
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
