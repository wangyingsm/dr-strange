//! `drsg update` — ask GitHub whether there is a newer release, and if there
//! is, hand this process over to the installer.
//!
//! ## Why it hands over rather than does the work
//!
//! A self-updater that downloads and swaps its own binary has to get several
//! awkward things right — the target triple, the checksum, an atomic replace
//! of the file it is executing from, a `PATH` that may hold two copies — and
//! `scripts/install.sh` already gets all of them right, because it is the
//! documented way in. Reimplementing it here would mean two installers to keep
//! in step, and the second one only ever exercised by people upgrading.
//!
//! So this command does the one thing the installer cannot: decide *whether*
//! to run. Then it `exec`s the installer, which is the same
//! `curl … | sh` a first-time install runs. `exec`, not spawn: there is
//! nothing left for this process to do afterwards, and the installer is about
//! to overwrite the very file it was loaded from — a parent waiting around to
//! print "done" would be waiting in a binary that no longer exists on disk.
//! The exit status the user sees is the installer's own.
//!
//! ## What it checks
//!
//! `github.com/<repo>/releases/latest` redirects to the newest tag, and the
//! `Location` header names it. That is the installer's own trick, for the
//! installer's own reason: the releases API is rate-limited for
//! unauthenticated callers, and a version check that fails on a shared address
//! would be worse than none.
//!
//! The comparison is numeric, not textual, and it is an ordering rather than
//! an inequality — a build *ahead* of the latest release (a `cargo install`
//! from master) must be told it is ahead, not talked into a downgrade.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};

/// This build, from the workspace version — the same string `drsg --version`
/// prints.
const CURRENT: &str = env!("CARGO_PKG_VERSION");

const REPO: &str = "wangyingsm/dr-strange";

/// Where the installer is read from: the default branch, as the README's
/// one-liner does. Following `master` rather than a tag is deliberate — a fix
/// to the installer should reach an upgrade the day it lands, and the archive
/// it installs is chosen by the release it resolves, not by this file.
const INSTALLER: &str =
    "https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.sh";

/// The Windows equivalent, named only in the message this command prints there
/// instead of running anything.
#[cfg(not(unix))]
const INSTALLER_PS1: &str =
    "https://raw.githubusercontent.com/wangyingsm/dr-strange/master/scripts/install.ps1";

/// What the check found.
#[derive(Debug, PartialEq, Eq)]
pub enum Standing {
    /// A newer release exists.
    Behind { latest: String },
    /// This build is the latest release.
    Current,
    /// This build is newer than any release — built from source, or from a
    /// branch ahead of the last tag.
    Ahead { latest: String },
}

/// The MCP server's binary, looked for beside this one when no `--bin` was
/// given: an install that has both should not be left half-upgraded.
const MCP_BIN: &str = "drsg-mcp";

/// `bin` is what `--bin` asked for; `None` means nothing was asked, and the
/// choice is made by what is installed beside this binary (see [`choose_bin`]).
pub fn update(
    allow_private: &[dr_strange_web::fetch::Prefix],
    bin: Option<&str>,
    dir: Option<&Path>,
    out: &mut dyn Write,
) -> Result<()> {
    let latest = latest_release(allow_private)?;
    match standing(CURRENT, &latest) {
        Standing::Current => {
            writeln!(out, "drsg {CURRENT} is the latest release — nothing to do")?;
            Ok(())
        }
        Standing::Ahead { latest } => {
            // Not an error, and not an upgrade: reinstalling would move the
            // user *backwards*, which is never what `update` meant.
            writeln!(
                out,
                "drsg {CURRENT} is ahead of the latest release ({latest}) — \
                 built from source, presumably. Nothing to do."
            )?;
            Ok(())
        }
        Standing::Behind { latest } => {
            writeln!(out, "drsg {CURRENT} -> {latest}")?;
            let dir = match dir {
                Some(d) => d.to_path_buf(),
                None => install_dir()?,
            };
            let bin = match bin {
                Some(asked) => asked.to_string(),
                None => choose_bin(&dir, out)?,
            };
            install(&bin, &dir, out)
        }
    }
}

/// What to update when nobody said: `drsg`, and `drsg-mcp` with it when the
/// two are installed side by side.
///
/// The two binaries are one release. An agent host that launches the
/// `drsg-mcp` next to an upgraded `drsg` would otherwise keep speaking last
/// release's tool set against this release's server — and nothing would say
/// so, since each binary is individually fine. Naming `--bin` explicitly still
/// means exactly what it names.
fn choose_bin(dir: &Path, out: &mut dyn Write) -> Result<String> {
    let mcp = dir.join(MCP_BIN);
    if mcp.is_file() {
        writeln!(out, "{} is beside it — updating both", mcp.display())?;
        Ok("all".to_string())
    } else {
        Ok("drsg".to_string())
    }
}

/// The newest release's version, without the leading `v`.
pub fn latest_release(allow_private: &[dr_strange_web::fetch::Prefix]) -> Result<String> {
    let url = format!("https://github.com/{REPO}/releases/latest");
    let location = dr_strange_web::fetch::redirect_target(&url, allow_private)
        .context("asking GitHub for the latest release")?;
    tag_version(&location).with_context(|| {
        format!("could not read a release version out of GitHub's answer: {location}")
    })
}

/// Pull `2.2.1` out of `https://github.com/o/r/releases/tag/v2.2.1`.
///
/// Held to a shape rather than trusting the tail of a URL: this string decides
/// whether the process is about to replace itself, and "whatever came after the
/// last slash" is not a version.
fn tag_version(location: &str) -> Result<String> {
    let tag = location.rsplit('/').next().unwrap_or_default();
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let numeric = version.split('-').next().unwrap_or_default();
    let parts: Vec<&str> = numeric.split('.').collect();
    if parts.len() < 2
        || !parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        bail!("`{tag}` is not a release tag");
    }
    Ok(version.to_string())
}

/// Where `current` sits relative to `latest`.
///
/// A dotted numeric compare, with a pre-release sorting below the release it
/// leads to, so `2.3.0-dev` is behind `2.3.0` and ahead of `2.2.1`. (The plugin
/// catalog carries its own copy of this ordering, in `dr-strange-llm`; that
/// crate is optional here, and a `--no-default-features` build still has to be
/// able to update itself.)
pub fn standing(current: &str, latest: &str) -> Standing {
    use std::cmp::Ordering;
    match cmp_version(current, latest) {
        Ordering::Less => Standing::Behind {
            latest: latest.to_string(),
        },
        Ordering::Equal => Standing::Current,
        Ordering::Greater => Standing::Ahead {
            latest: latest.to_string(),
        },
    }
}

fn cmp_version(a: &str, b: &str) -> std::cmp::Ordering {
    fn key(v: &str) -> (Vec<u64>, bool) {
        let (core, pre) = match v.split_once(['-', '+']) {
            Some((core, _)) => (core, true),
            None => (v, false),
        };
        let parts = core
            .split('.')
            .map(|p| p.trim().parse::<u64>().unwrap_or(0))
            .collect();
        (parts, pre)
    }
    let (ka, pre_a) = key(a);
    let (kb, pre_b) = key(b);
    for i in 0..ka.len().max(kb.len()) {
        let x = ka.get(i).copied().unwrap_or(0);
        let y = kb.get(i).copied().unwrap_or(0);
        if x != y {
            return x.cmp(&y);
        }
    }
    pre_b.cmp(&pre_a)
}

/// Hand this process over to the installer.
fn install(bin: &str, dir: &Path, out: &mut dyn Write) -> Result<()> {
    let command = installer_command(bin, dir)?;
    // Printed before the handover, because after it this process is gone: if
    // the installer fails, or the network dies mid-download, the line above
    // the wreckage is the command to retry by hand.
    writeln!(out, "$ {command}")?;
    out.flush()?;

    exec(&command)
}

/// The shell one-liner: the README's install command, with this binary's own
/// location and name filled in.
fn installer_command(bin: &str, dir: &Path) -> Result<String> {
    let dir = dir.to_str().with_context(|| {
        format!(
            "{} is not valid UTF-8; pass --dir with a plain path",
            dir.display()
        )
    })?;
    // Single-quoted for the shell, with any embedded quote escaped the POSIX
    // way. A path is the one part of this command a user controls.
    let quoted = format!("'{}'", dir.replace('\'', r"'\''"));
    let downloader = if which("curl").is_some() {
        format!("curl -fsSL {INSTALLER}")
    } else if which("wget").is_some() {
        format!("wget -qO- {INSTALLER}")
    } else {
        bail!(
            "neither curl nor wget is available — install manually from \
             https://github.com/{REPO}/releases"
        )
    };
    Ok(format!(
        "{downloader} | sh -s -- --bin {bin} --dir {quoted}"
    ))
}

/// The directory to install into: the one this binary is running from.
///
/// Not the installer's `~/.local/bin` default. An operator whose `drsg` lives
/// in `/usr/local/bin` would otherwise get a second, newer copy in a directory
/// that may not even be on `PATH` — and would keep running the old one while
/// being told the update succeeded. A version check that ends in two versions
/// is worse than no version check.
fn install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding this binary's own path")?;
    // Canonicalized so a symlinked `drsg` updates the real file's directory
    // rather than dropping a binary beside the link.
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent()
        .map(Path::to_path_buf)
        .with_context(|| format!("{} has no parent directory", exe.display()))
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Replace this process with `sh -c <command>`.
#[cfg(unix)]
fn exec(command: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // `exec` returns only on failure — on success this process no longer
    // exists to return into.
    let e = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .exec();
    Err(anyhow::Error::new(e).context("could not run the installer"))
}

/// Windows cannot do this, and says so rather than failing halfway.
///
/// A running `.exe` is locked against being overwritten, so the installer
/// would download the archive and then fail on the copy — after having claimed
/// to start. Printing the command and standing down leaves the user one
/// paste away, in a shell where this process is no longer running.
#[cfg(not(unix))]
fn exec(_command: &str) -> Result<()> {
    bail!(
        "drsg cannot replace itself while it is running on this platform — \
         Windows locks the running executable. Run the installer from a \
         terminal instead:\n  irm {INSTALLER_PS1} | iex"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tag_is_read_out_of_githubs_redirect() {
        assert_eq!(
            tag_version("https://github.com/wangyingsm/dr-strange/releases/tag/v2.2.1").unwrap(),
            "2.2.1"
        );
        // No `v`, and a pre-release, are both real tags.
        assert_eq!(tag_version("https://x/tag/2.3.0").unwrap(), "2.3.0");
        assert_eq!(
            tag_version("https://x/tag/v2.0.0-alpha").unwrap(),
            "2.0.0-alpha"
        );
    }

    /// The answer decides whether this process replaces itself, so anything
    /// that is not a version has to be refused rather than shrugged at.
    #[test]
    fn anything_that_is_not_a_release_tag_is_refused() {
        for junk in [
            // GitHub answering with the releases page: no tag at all.
            "https://github.com/wangyingsm/dr-strange/releases",
            "https://x/tag/latest",
            "https://x/tag/v",
            "https://x/tag/vNext",
            "https://x/tag/v2",
            "https://x/tag/v2..1",
            "",
        ] {
            assert!(tag_version(junk).is_err(), "accepted {junk:?}");
        }
    }

    #[test]
    fn versions_compare_by_component_not_by_string() {
        assert_eq!(
            standing("2.2.1", "2.10.0"),
            Standing::Behind {
                latest: "2.10.0".into()
            }
        );
        assert_eq!(standing("2.2.1", "2.2.1"), Standing::Current);
    }

    /// The case that makes this an ordering rather than a `!=`: a build from
    /// master must not be talked into installing an older release over itself.
    #[test]
    fn a_build_ahead_of_the_latest_release_is_not_an_update() {
        assert_eq!(
            standing("2.3.0", "2.2.1"),
            Standing::Ahead {
                latest: "2.2.1".into()
            }
        );
        // A pre-release is behind its own release and ahead of the last one.
        assert_eq!(
            standing("2.3.0-dev", "2.3.0"),
            Standing::Behind {
                latest: "2.3.0".into()
            }
        );
        assert_eq!(
            standing("2.3.0-dev", "2.2.1"),
            Standing::Ahead {
                latest: "2.2.1".into()
            }
        );
    }

    /// An install that has both binaries is upgraded as one; an install that
    /// has only `drsg` is not made to download a server it never had.
    #[test]
    fn a_drsg_mcp_beside_this_binary_is_updated_with_it() {
        let dir = std::env::temp_dir().join(format!("drsg-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut out = Vec::new();
        assert_eq!(choose_bin(&dir, &mut out).unwrap(), "drsg");
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));

        std::fs::write(dir.join(MCP_BIN), b"").unwrap();
        let mut out = Vec::new();
        assert_eq!(choose_bin(&dir, &mut out).unwrap(), "all");
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("drsg-mcp is beside it"), "{said}");

        // A directory of that name is not the binary.
        std::fs::remove_file(dir.join(MCP_BIN)).unwrap();
        std::fs::create_dir(dir.join(MCP_BIN)).unwrap();
        assert_eq!(choose_bin(&dir, &mut Vec::new()).unwrap(), "drsg");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_command_names_this_binarys_own_directory() {
        let cmd = installer_command("drsg", Path::new("/usr/local/bin")).unwrap();
        assert!(cmd.contains("scripts/install.sh"), "{cmd}");
        assert!(
            cmd.ends_with("| sh -s -- --bin drsg --dir '/usr/local/bin'"),
            "{cmd}"
        );
    }

    /// A path is the one part of this command someone else chooses, and it is
    /// about to be handed to `sh -c`.
    #[test]
    fn a_directory_cannot_break_out_of_the_shell_command() {
        let cmd = installer_command("drsg", Path::new("/tmp/a'; rm -rf /; echo '")).unwrap();
        assert!(
            cmd.ends_with(r"--dir '/tmp/a'\''; rm -rf /; echo '\'''"),
            "{cmd}"
        );
        // Every quote is paired: the injected text stays one argument.
        assert_eq!(cmd.matches('\'').count() % 2, 0, "{cmd}");
    }
}
