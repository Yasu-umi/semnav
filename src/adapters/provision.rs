//! LSP server provisioning — detect on `PATH`, reuse an isolated npm install,
//! or install fresh into `<cache_dir>/servers`.
//!
//! Algorithm (`docs/design/language-adapters.md` "LSP Server Provisioning"):
//! 1. If the server binary is on `PATH`, use it (the child inherits `PATH`).
//! 2. Else if a prior isolated install left it in `<servers_dir>/node_modules/.bin`, reuse it.
//! 3. Else `npm install --prefix <servers_dir> <pkg>@<ver> [<peer>@<ver>…]`, then use the
//!    freshly-created `.bin` entry. If Node.js/npm is missing, surface a clear install hint
//!    rather than auto-installing a runtime.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::fs;
use tokio::process::Command;

use crate::adapters::{CommandSpec, LanguageAdapter, ServerPackage};

/// Where isolated LSP server installs live: `<cache_dir>/servers`.
#[derive(Debug, Clone)]
pub struct ProvisionContext {
    pub servers_dir: PathBuf,
}

/// Where a server binary was resolved, independent of how it got there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolved {
    /// Found on `PATH`; the child resolves it via the inherited environment.
    OnPath,
    /// Already installed in the isolated dir; spawn this absolute path.
    Isolated(PathBuf),
    /// Nowhere yet; needs an `npm install`.
    NeedsInstall,
}

/// Absolute path of a previously-installed server binary under `servers_dir`.
fn isolated_bin(servers_dir: &Path, program: &str) -> PathBuf {
    servers_dir.join("node_modules").join(".bin").join(program)
}

/// Resolve the server binary: `PATH` first, then the isolated `.bin`, else install.
fn resolve_binary(program: &str, servers_dir: &Path) -> Resolved {
    if which::which(program).is_ok() {
        return Resolved::OnPath;
    }
    let installed = isolated_bin(servers_dir, program);
    if installed.exists() {
        return Resolved::Isolated(installed);
    }
    Resolved::NeedsInstall
}

/// Build the spawnable command for `spec` pointing at `program` (bare name or path).
fn build_command(spec: CommandSpec, program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(spec.args);
    cmd
}

/// Fail fast with an install hint if the Node.js/npm runtime semnav rides on is absent.
fn require_runtime(program: &str) -> Result<()> {
    if which::which("node").is_err() {
        bail!(
            "Node.js not found on PATH; install it to provision the `{program}` \
             language server (see https://nodejs.org)"
        );
    }
    if which::which("npm").is_err() {
        bail!(
            "npm not found on PATH; it ships with Node.js — install Node.js to \
             provision the `{program}` language server"
        );
    }
    Ok(())
}

/// `npm install --prefix <servers_dir> <pkg>@<ver> [<peer>@<ver>…]` into an isolated
/// `node_modules`. Stdio is inherited so install progress is visible; no internal
/// timeout (a cold install can exceed the handshake timeout).
async fn npm_install(pkg: &ServerPackage, servers_dir: &Path, program: &str) -> Result<()> {
    require_runtime(program)?;
    fs::create_dir_all(servers_dir)
        .await
        .with_context(|| format!("failed to create servers dir {}", servers_dir.display()))?;

    let prefix = servers_dir.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec![
        "install".into(),
        "--prefix".into(),
        prefix,
        format!("{}@{}", pkg.npm_package, pkg.version),
    ];
    for peer in pkg.peers {
        args.push((*peer).to_string());
    }

    let status = Command::new("npm")
        .args(&args)
        // Inherit stdio so the user sees npm fetch/install progress live.
        .kill_on_drop(true)
        .status()
        .await
        .with_context(|| "failed to spawn npm")?;
    if !status.success() {
        bail!(
            "npm install for `{program}` failed with status {status}; \
             see npm output above"
        );
    }
    Ok(())
}

/// Provision (or reuse) the language server for `adapter` and return a ready-to-spawn
/// [`Command`] with its `--stdio` (etc.) args already applied.
pub async fn provision(adapter: &dyn LanguageAdapter, ctx: &ProvisionContext) -> Result<Command> {
    let spec = adapter.server_command();
    let resolved = resolve_binary(spec.program, &ctx.servers_dir);
    let program = match resolved {
        Resolved::OnPath => spec.program.to_string(),
        Resolved::Isolated(path) => path.to_string_lossy().into_owned(),
        Resolved::NeedsInstall => match adapter.server_package() {
            Some(pkg) => {
                npm_install(&pkg, &ctx.servers_dir, spec.program).await?;
                let path = isolated_bin(&ctx.servers_dir, spec.program);
                if !path.exists() {
                    bail!(
                        "npm install completed but {} was not created; the `{}` package may \
                         not provide the `{}` binary",
                        path.display(),
                        pkg.npm_package,
                        spec.program
                    );
                }
                path.to_string_lossy().into_owned()
            }
            None => bail!(
                "`{program}` not found on PATH; this language server isn't auto-installable \
                 — install it manually (e.g. `rustup component add rust-analyzer`) and ensure \
                 it's on PATH",
                program = spec.program,
            ),
        },
    };
    Ok(build_command(spec, &program))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// `true` exists on PATH on both darwin and linux → resolution must prefer PATH.
    #[test]
    fn resolve_binary_prefers_path() {
        let dir = tempdir().expect("tempdir");
        assert_eq!(resolve_binary("true", dir.path()), Resolved::OnPath);
    }

    /// A name not on PATH but present in the isolated `.bin` resolves to that path.
    #[test]
    fn resolve_binary_reuses_isolated_install() {
        let dir = tempdir().expect("tempdir");
        let bin = isolated_bin(dir.path(), "semnav-provision-test-server");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, "#!/bin/sh\n").unwrap();

        assert_eq!(
            resolve_binary("semnav-provision-test-server", dir.path()),
            Resolved::Isolated(bin)
        );
    }

    /// Neither on PATH nor isolated → needs an install.
    #[test]
    fn resolve_binary_needs_install_when_absent() {
        let dir = tempdir().expect("tempdir");
        assert_eq!(
            resolve_binary("semnav-provision-absent-server", dir.path()),
            Resolved::NeedsInstall
        );
    }

    #[test]
    fn build_command_applies_args() {
        // The program/args are observable via the underlying std Command.
        let cmd = build_command(
            CommandSpec {
                program: "pyright-langserver",
                args: &["--stdio"],
            },
            "/abs/pyright-langserver",
        );
        let std_cmd = cmd.as_std();
        assert_eq!(
            std_cmd.get_program(),
            std::ffi::OsStr::new("/abs/pyright-langserver")
        );
        let args: Vec<&std::ffi::OsStr> = std_cmd.get_args().collect();
        assert_eq!(args, vec![std::ffi::OsStr::new("--stdio")]);
    }
}
