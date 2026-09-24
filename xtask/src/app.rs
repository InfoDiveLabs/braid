//! Launching `dl-gui` headless under the Slint MCP server.

use anyhow::{Context, Result, bail};
use std::process::{Child, Command, Stdio};

/// A `dl-gui` process running windowless with its MCP endpoint open.
pub struct HeadlessApp {
    child: Child,
    pub mcp: crate::mcp::Mcp,
}

impl HeadlessApp {
    /// Build a `dl-gui` example with the features MCP introspection needs.
    ///
    /// `slint/mcp` is passed on the command line rather than declared in
    /// `dl-gui`'s `[features]`, per Slint's own guidance: it keeps the MCP
    /// server out of any build that does not explicitly ask for it.
    pub fn build_example(release: bool, example: &str) -> Result<std::path::PathBuf> {
        Self::build_target(release, Some(example), example)
    }

    /// Build the application itself, not a fixture.
    ///
    /// The screenshot fixture wires only what it needs, so a control that the
    /// real binary connects and the fixture does not looks dead when driven
    /// there. Anything whose wiring is the thing under test has to be checked
    /// against this.
    pub fn build_app(release: bool) -> Result<std::path::PathBuf> {
        Self::build_target(release, None, "braid")
    }

    fn build_target(
        release: bool,
        example: Option<&str>,
        artifact: &str,
    ) -> Result<std::path::PathBuf> {
        let mut cmd = Command::new(cargo());
        cmd.args(["build", "-p", "dl-gui", "--features", "devtools,slint/mcp"]);
        if let Some(example) = example {
            cmd.args(["--example", example]);
        }
        if release {
            cmd.arg("--release");
        }
        let status = cmd.status().context("running cargo build")?;
        if !status.success() {
            bail!("cargo build failed");
        }
        let profile = if release { "release" } else { "debug" };
        let mut path = workspace_root().join("target").join(profile);
        if example.is_some() {
            path.push("examples");
        }
        Ok(path.join(artifact))
    }

    pub fn launch_with_args(binary: &std::path::Path, port: u16, args: &[&str]) -> Result<Self> {
        let child = Command::new(binary)
            .args(args)
            // The headless backend software-rasterizes with no display server,
            // so this works in CI and in a sandbox. Set it explicitly rather
            // than relying on Slint's auto-fallback, which would also mask a
            // genuine failure of the real winit backend.
            .env("SLINT_BACKEND", "headless")
            .env("SLINT_MCP_PORT", port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("launching {}", binary.display()))?;

        let mcp = crate::mcp::Mcp::new(port);
        let app = Self { child, mcp };
        app.mcp.wait_ready(std::time::Duration::from_secs(30))?;
        Ok(app)
    }
}

impl Drop for HeadlessApp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

pub fn workspace_root() -> std::path::PathBuf {
    // xtask/src/app.rs -> xtask/src -> xtask -> <root>
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}
