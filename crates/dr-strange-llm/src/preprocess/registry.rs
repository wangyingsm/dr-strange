//! Where installed plugins live, and what is known about them (ROADMAP §11).
//!
//! The store is per-user — `$XDG_DATA_HOME/drsg/plugins`, usually
//! `~/.local/share/drsg/plugins` — holding the wasm files and a
//! `registry.toml` recording what each one is:
//!
//! ```toml
//! [[plugin]]
//! name = "toml"
//! version = "1"
//! file = "toml-1.wasm"
//! sha256 = "9f86d081…"
//! source = "/home/me/build/drsg_plugin_toml.wasm"
//! extensions = ["toml"]
//! compiled = "toml-1.cwasm"
//! compiled_sha256 = "2c26b46b…"
//! ```
//!
//! ## Identity is the hash
//!
//! A plugin's SHA-256 is recorded at install and **re-checked at every load**,
//! so a file that changed on disk is refused rather than silently run — this
//! settles §11's open "plugin identity" fork. The manifest is likewise asked
//! of the component itself at load rather than trusted from this file, so a
//! record that drifted from its artifact is caught, not believed.
//!
//! Install validates before it stores: the bytes must be a component, must not
//! import a forbidden interface, and must describe themselves — all the checks
//! [`WasmPlugin::from_bytes`] performs — so nothing unloadable ever enters the
//! store to fail later at digest time.
//!
//! ## Compiled once
//!
//! Compiling a component is the expensive half of a load — seconds of CPU and
//! hundreds of MiB for the official parsers — and a load happens on every
//! `digest` and on every commit `serve watch` folds. So install also stores
//! the **compiled form** beside the wasm (`compiled`), pinned by its own
//! hash, and a load deserializes that in milliseconds. The pin is what makes
//! deserializing safe: a compiled artifact is native code, so it is trusted
//! only as far as the registry this store wrote can vouch for it, the same
//! way the wasm is.
//!
//! The artifact is tied to the wasmtime build that made it. After a `drsg
//! update`, or in a store from before artifacts existed, a load compiles
//! from the wasm as before and writes the artifact for the next one — a
//! warning if that write fails, never an error, since the plugin loaded
//! either way. With fuel metering turned off (`[plugins] fuel = 0`) the
//! artifact does not apply — it is compiled metered — and every load
//! compiles.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::Preprocessor;
use super::wasm::{Limits, WasmPlugin};

/// One installed plugin, as `registry.toml` records it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstalledPlugin {
    pub name: String,
    pub version: String,
    /// The wasm file, relative to the store directory.
    pub file: String,
    /// Hex SHA-256 of the file, pinned at install.
    pub sha256: String,
    /// Where it came from — a path or a URL, for `plugin list` to show.
    pub source: String,
    /// Cached from `describe()` so routing can be answered without
    /// instantiating every plugin; re-checked against the component at load.
    pub extensions: Vec<String>,
    /// The manifest's inline SVG, cached at install for UIs to show without
    /// instantiating the component. Absent means the UI's default mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    /// The compiled form of `file`, relative to the store directory — what a
    /// load deserializes instead of compiling. Absent on a record from before
    /// artifacts existed, or when writing it failed; the next load fills it in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiled: Option<String>,
    /// Hex SHA-256 of `compiled`, pinned when it was written; a load re-checks
    /// it before deserializing, since that runs the bytes as native code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiled_sha256: Option<String>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct RegistryFile {
    #[serde(default, rename = "plugin")]
    plugins: Vec<InstalledPlugin>,
}

/// The plugin store: a directory of wasm files and the registry beside them.
pub struct PluginStore {
    dir: PathBuf,
}

impl PluginStore {
    /// The per-user store, created if absent.
    pub fn open_default() -> Result<Self> {
        Self::open(default_dir()?)
    }

    /// A store at an explicit directory — how tests get one that is theirs.
    pub fn open(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating the plugin store at {}", dir.display()))?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Validate, hash, store, record. Returns the new entry and, when a plugin
    /// of the same name was already installed, the version it replaced —
    /// installing again *is* the upgrade path.
    pub fn install(&self, bytes: &[u8], source: &str) -> Result<(InstalledPlugin, Option<String>)> {
        // Everything `from_bytes` checks — a real component, no forbidden
        // imports, a manifest — is checked *before* anything is stored, so the
        // store never holds a file that will fail at digest time.
        let plugin = WasmPlugin::from_bytes(bytes, Vec::new(), Limits::default())
            .context("validating the plugin before installing it")?;
        let manifest = plugin.manifest();

        // The name becomes a filename and a config key, so it is held to
        // characters that are safe as both.
        if !manifest
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            bail!(
                "plugin name `{}` may only contain lowercase letters, digits, \
                 `-` and `_` — it becomes a filename and a config section",
                manifest.name
            );
        }

        let sha256 = hex_sha256(bytes);
        let file = format!("{}-{}.wasm", manifest.name, manifest.version);
        let path = self.dir.join(&file);

        // Write-then-rename, so a crash mid-write cannot leave a half plugin
        // under a name the registry points at.
        let tmp = self.dir.join(format!(".{file}.tmp"));
        std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("moving the plugin into {}", path.display()))?;

        // The compiled form beside it. Best-effort: the plugin is installed
        // either way, and a load that finds no artifact compiles and writes
        // one itself.
        let (compiled, compiled_sha256) =
            match self.write_artifact(&manifest.name, &manifest.version, &plugin) {
                Ok((file, sha256)) => (Some(file), Some(sha256)),
                Err(e) => {
                    tracing::warn!(
                        plugin = %manifest.name,
                        error = format!("{e:#}"),
                        "storing the compiled plugin failed; it will be compiled at load"
                    );
                    (None, None)
                }
            };

        let entry = InstalledPlugin {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            file,
            sha256,
            source: source.to_string(),
            extensions: manifest.extensions.clone(),
            logo: manifest.logo.clone(),
            compiled,
            compiled_sha256,
        };

        let mut registry = self.read()?;
        let replaced = match registry.plugins.iter_mut().find(|p| p.name == entry.name) {
            Some(existing) => {
                let old = std::mem::replace(existing, entry.clone());
                // A different version means a different filename; the old file
                // would otherwise linger as an orphan nothing points at.
                if old.file != existing.file {
                    let _ = std::fs::remove_file(self.dir.join(&old.file));
                }
                if let Some(stale) = old
                    .compiled
                    .filter(|c| Some(c) != existing.compiled.as_ref())
                {
                    let _ = std::fs::remove_file(self.dir.join(stale));
                }
                Some(old.version)
            }
            None => {
                registry.plugins.push(entry.clone());
                None
            }
        };
        self.write(&registry)?;
        Ok((entry, replaced))
    }

    pub fn list(&self) -> Result<Vec<InstalledPlugin>> {
        Ok(self.read()?.plugins)
    }

    /// Remove a plugin by name: the file and the record together.
    pub fn remove(&self, name: &str) -> Result<InstalledPlugin> {
        let mut registry = self.read()?;
        let idx = registry
            .plugins
            .iter()
            .position(|p| p.name == name)
            .with_context(|| {
                let known: Vec<&str> = registry.plugins.iter().map(|p| p.name.as_str()).collect();
                format!(
                    "no plugin named `{name}` is installed (installed: {})",
                    if known.is_empty() {
                        "none".to_string()
                    } else {
                        known.join(", ")
                    }
                )
            })?;
        let entry = registry.plugins.remove(idx);
        self.write(&registry)?;
        let _ = std::fs::remove_file(self.dir.join(&entry.file));
        if let Some(compiled) = &entry.compiled {
            let _ = std::fs::remove_file(self.dir.join(compiled));
        }
        Ok(entry)
    }

    /// Load every installed plugin, verifying each file still hashes to what
    /// was pinned at install.
    ///
    /// A failure is an error naming the plugin, never a silent skip: the
    /// operator installed it, so a digest quietly running without it would be
    /// the worst of the options.
    ///
    /// A plugin that had to be compiled (no artifact, a stale one, or one from
    /// another build) gets its artifact written and recorded here, so the
    /// next load deserializes. Recording it is best-effort: the plugins are
    /// loaded either way, and a store that cannot be written right now just
    /// compiles again next time.
    pub fn load_all(
        &self,
        options: &BTreeMap<String, Vec<(String, String)>>,
        limits: &Limits,
    ) -> Result<Vec<WasmPlugin>> {
        let mut registry = self.read()?;
        let mut out = Vec::with_capacity(registry.plugins.len());
        let mut recompiled = false;
        for entry in &mut registry.plugins {
            let (plugin, compiled_now) = self.load_one(entry, options, limits)?;
            recompiled |= compiled_now;
            out.push(plugin);
        }
        if recompiled && let Err(e) = self.write(&registry) {
            tracing::warn!(
                error = format!("{e:#}"),
                "recording the compiled plugins failed; the next load compiles them again"
            );
        }
        Ok(out)
    }

    /// One plugin, from its artifact when there is a usable one and from the
    /// wasm otherwise. The flag says the wasm was compiled and `entry` now
    /// names a fresh artifact the caller should record.
    fn load_one(
        &self,
        entry: &mut InstalledPlugin,
        options: &BTreeMap<String, Vec<(String, String)>>,
        limits: &Limits,
    ) -> Result<(WasmPlugin, bool)> {
        let path = self.dir.join(&entry.file);
        let bytes = std::fs::read(&path).with_context(|| {
            format!(
                "plugin `{}` is registered but {} is unreadable — \
                 `drsg plugin remove {}` clears the record",
                entry.name,
                path.display(),
                entry.name
            )
        })?;

        let found = hex_sha256(&bytes);
        if found != entry.sha256 {
            bail!(
                "plugin `{}` changed on disk since it was installed\n  \
                 expected sha256:{}\n  found    sha256:{}\n\
                 reinstall it to accept the new file",
                entry.name,
                entry.sha256,
                found
            );
        }

        let opts = options.get(&entry.name).cloned().unwrap_or_default();
        let mut recompiled = false;
        let plugin = match self.load_precompiled(entry, opts.clone(), limits) {
            Some(plugin) => plugin,
            None => {
                let plugin = WasmPlugin::from_bytes(&bytes, opts, limits.clone())
                    .with_context(|| format!("loading plugin `{}`", entry.name))?;
                // Metered only: an unmetered engine cannot load the artifact,
                // and one written from it could not be loaded by a metered one.
                if limits.fuel.is_some() {
                    match self.write_artifact(&entry.name, &entry.version, &plugin) {
                        Ok((file, sha256)) => {
                            entry.compiled = Some(file);
                            entry.compiled_sha256 = Some(sha256);
                            recompiled = true;
                        }
                        Err(e) => tracing::warn!(
                            plugin = %entry.name,
                            error = format!("{e:#}"),
                            "storing the compiled plugin failed; it will be compiled on every load"
                        ),
                    }
                }
                plugin
            }
        };

        // The component's own answer outranks the record. A name mismatch is
        // an integrity failure; extensions merely drifted are taken from the
        // component, which is the authority on itself.
        let manifest = plugin.manifest();
        if manifest.name != entry.name {
            bail!(
                "the file registered as `{}` describes itself as `{}` — the \
                 registry record has drifted from the artifact",
                entry.name,
                manifest.name
            );
        }
        Ok((plugin, recompiled))
    }

    /// The plugin from its recorded artifact, or `None` with the reason logged
    /// when the wasm has to be compiled instead: no artifact recorded, the
    /// file gone, its hash off the pin, or wasmtime refusing it (another
    /// build made it). Fuel off means no artifact applies — see
    /// [`WasmPlugin::from_precompiled`].
    fn load_precompiled(
        &self,
        entry: &InstalledPlugin,
        opts: Vec<(String, String)>,
        limits: &Limits,
    ) -> Option<WasmPlugin> {
        // Artifacts are metered; an unmetered engine cannot load one.
        limits.fuel?;
        let (file, pinned) = match (&entry.compiled, &entry.compiled_sha256) {
            (Some(file), Some(pinned)) => (file, pinned),
            _ => return None,
        };
        let path = self.dir.join(file);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::info!(
                    plugin = %entry.name,
                    path = %path.display(),
                    error = %e,
                    "the compiled plugin is unreadable; compiling from the wasm"
                );
                return None;
            }
        };
        if hex_sha256(&bytes) != *pinned {
            // Derived data, so rebuilt rather than refused — but said out
            // loud, because an artifact that changed by itself is worth a look.
            tracing::warn!(
                plugin = %entry.name,
                path = %path.display(),
                "the compiled plugin changed on disk since it was written; compiling from the wasm and replacing it"
            );
            return None;
        }
        match WasmPlugin::from_precompiled(&bytes, opts, limits.clone()) {
            Ok(plugin) => Some(plugin),
            Err(e) => {
                tracing::info!(
                    plugin = %entry.name,
                    error = format!("{e:#}"),
                    "the compiled plugin is from another drsg build; compiling from the wasm and replacing it"
                );
                None
            }
        }
    }

    /// Write `plugin`'s compiled form beside its wasm — write-then-rename, as
    /// the wasm itself — and return what the registry records for it:
    /// `(file, sha256)`.
    fn write_artifact(
        &self,
        name: &str,
        version: &str,
        plugin: &WasmPlugin,
    ) -> Result<(String, String)> {
        let bytes = plugin.serialize()?;
        let file = format!("{name}-{version}.cwasm");
        let path = self.dir.join(&file);
        let tmp = self.dir.join(format!(".{file}.tmp"));
        std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("moving the compiled plugin into {}", path.display()))?;
        Ok((file, hex_sha256(&bytes)))
    }

    fn read(&self) -> Result<RegistryFile> {
        let path = self.dir.join("registry.toml");
        if !path.exists() {
            return Ok(RegistryFile::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    fn write(&self, registry: &RegistryFile) -> Result<()> {
        let path = self.dir.join("registry.toml");
        let text = toml::to_string_pretty(registry).context("rendering registry.toml")?;
        let tmp = self.dir.join(".registry.toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).context("moving registry.toml into place")?;
        Ok(())
    }
}

/// `$XDG_DATA_HOME/drsg/plugins`, or `~/.local/share/drsg/plugins`.
fn default_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("drsg").join("plugins"));
    }
    let home = std::env::home_dir()
        .context("neither $XDG_DATA_HOME nor a home directory — set XDG_DATA_HOME")?;
    Ok(home
        .join(".local")
        .join("share")
        .join("drsg")
        .join("plugins"))
}

pub(super) fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}
