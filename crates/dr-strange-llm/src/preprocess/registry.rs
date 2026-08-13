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

        let entry = InstalledPlugin {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            file,
            sha256,
            source: source.to_string(),
            extensions: manifest.extensions.clone(),
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
        Ok(entry)
    }

    /// Load every installed plugin, verifying each file still hashes to what
    /// was pinned at install.
    ///
    /// A failure is an error naming the plugin, never a silent skip: the
    /// operator installed it, so a digest quietly running without it would be
    /// the worst of the options.
    pub fn load_all(
        &self,
        options: &BTreeMap<String, Vec<(String, String)>>,
        limits: &Limits,
    ) -> Result<Vec<WasmPlugin>> {
        self.read()?
            .plugins
            .iter()
            .map(|entry| self.load_one(entry, options, limits))
            .collect()
    }

    fn load_one(
        &self,
        entry: &InstalledPlugin,
        options: &BTreeMap<String, Vec<(String, String)>>,
        limits: &Limits,
    ) -> Result<WasmPlugin> {
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
        let plugin = WasmPlugin::from_bytes(&bytes, opts, limits.clone())
            .with_context(|| format!("loading plugin `{}`", entry.name))?;

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
        Ok(plugin)
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

fn hex_sha256(bytes: &[u8]) -> String {
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
