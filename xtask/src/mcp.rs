//! Minimal client for Slint's embedded MCP server.
//!
//! Slint 1.18 exposes an MCP Streamable-HTTP endpoint from the testing backend
//! when built with `--features slint/mcp` and run with `SLINT_MCP_PORT` set.
//! It speaks JSON-RPC 2.0 over `POST /mcp`, bound to loopback only.
//!
//! Two things are easy to get wrong and are handled here:
//!   * Window handles and element handles are structurally identical
//!     (`{index, generation}`) but are NOT interchangeable.
//!   * Element introspection returns `debugInfoMissing: true` unless the UI was
//!     *compiled* with debug info: see `dl-gui/build.rs`, which ties that to
//!     the `devtools` feature.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};

pub struct Mcp {
    url: String,
    next_id: std::cell::Cell<u64>,
}

/// An opaque `{index, generation}` pair. Kept distinct from [`ElementHandle`]
/// at the type level so the two cannot be passed to the wrong tool.
#[derive(Clone, Debug)]
pub struct WindowHandle(Value);

#[derive(Clone, Debug)]
pub struct ElementHandle(Value);

impl Mcp {
    pub fn new(port: u16) -> Self {
        Self { url: format!("http://127.0.0.1:{port}/mcp"), next_id: std::cell::Cell::new(0) }
    }

    /// Poll until the server answers, so callers need not guess a startup delay.
    pub fn wait_ready(&self, timeout: std::time::Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let mut last: Option<anyhow::Error> = None;
        while std::time::Instant::now() < deadline {
            match self.rpc("tools/list", json!({})) {
                Ok(_) => return Ok(()),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err(last.unwrap_or_else(|| anyhow!("timed out")))
            .context(format!("MCP server at {} never became ready", self.url))
    }

    fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});

        let mut resp = ureq::post(&self.url)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .with_context(|| format!("POST {} ({method})", self.url))?;
        let parsed: Value = resp.body_mut().read_json().context("decoding MCP response")?;

        if let Some(err) = parsed.get("error") {
            bail!("{method} failed: {err}");
        }
        parsed.get("result").cloned().ok_or_else(|| anyhow!("{method}: response had no result"))
    }

    /// Call a tool and parse its text content as JSON.
    fn call_json(&self, tool: &str, args: Value) -> Result<Value> {
        let result = self.rpc("tools/call", json!({"name": tool, "arguments": args}))?;
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|items| {
                items.iter().find(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            })
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{tool}: response had no text content"))?;
        serde_json::from_str(text).with_context(|| format!("{tool}: content was not JSON: {text}"))
    }

    pub fn first_window(&self) -> Result<WindowHandle> {
        let v = self.call_json("list_windows", json!({}))?;
        v["windowHandles"]
            .get(0)
            .cloned()
            .map(WindowHandle)
            .ok_or_else(|| anyhow!("the application reported no windows"))
    }

    pub fn root_element(&self, w: &WindowHandle) -> Result<ElementHandle> {
        let v = self.call_json("get_window_properties", json!({"windowHandle": w.0}))?;
        v.get("rootElementHandle")
            .cloned()
            .map(ElementHandle)
            .ok_or_else(|| anyhow!("window has no root element"))
    }

    pub fn element_tree(&self, root: &ElementHandle, max: u32) -> Result<Value> {
        let tree = self
            .call_json("get_element_tree", json!({"elementHandle": root.0, "maxElements": max}))?;
        if tree.get("debugInfoMissing").and_then(Value::as_bool) == Some(true) {
            bail!(
                "the UI was compiled without debug info, so element introspection is unavailable. \
                 Build with `--features devtools` (see dl-gui/build.rs)."
            );
        }
        Ok(tree)
    }

    /// Every element in the window, flattened.
    pub fn elements(&self, root: &ElementHandle) -> Result<Vec<Value>> {
        let tree = self.element_tree(root, 1000)?;
        Ok(tree.get("elements").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// Find an element by its accessible label.
    ///
    /// Repeated elements have no unique qualified id, so rows and their buttons
    /// are located the way a screen reader would: by what they say they are.
    pub fn find_by_label(
        &self,
        root: &ElementHandle,
        predicate: impl Fn(&str) -> bool,
    ) -> Result<Option<(String, ElementHandle)>> {
        for element in self.elements(root)? {
            let Some(label) = element.get("accessibleLabel").and_then(Value::as_str) else {
                continue;
            };
            if predicate(label)
                && let Some(handle) = element.get("handle")
            {
                return Ok(Some((label.to_string(), ElementHandle(handle.clone()))));
            }
        }
        Ok(None)
    }

    pub fn properties(&self, e: &ElementHandle) -> Result<Value> {
        self.call_json("get_element_properties", json!({"elementHandle": e.0}))
    }

    /// The accessible-value of an element. Elements a test asserts on must set
    /// `accessible-value` in the `.slint` source, or this returns `None`.
    pub fn accessible_value(&self, e: &ElementHandle) -> Result<Option<String>> {
        Ok(self.properties(e)?.get("accessibleValue").and_then(Value::as_str).map(str::to_owned))
    }

    pub fn click(&self, e: &ElementHandle) -> Result<()> {
        self.rpc(
            "tools/call",
            json!({"name": "click_element", "arguments": {"elementHandle": e.0}}),
        )?;
        Ok(())
    }

    pub fn screenshot(&self, w: &WindowHandle, out: &std::path::Path) -> Result<()> {
        let result = self.rpc(
            "tools/call",
            json!({"name": "take_screenshot", "arguments": {"windowHandle": w.0}}),
        )?;
        let data = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|items| {
                items.iter().find(|c| c.get("type").and_then(Value::as_str) == Some("image"))
            })
            .and_then(|c| c.get("data"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("take_screenshot returned no image"))?;

        let png = base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("screenshot was not valid base64")?;
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(out, png).with_context(|| format!("writing {}", out.display()))?;
        Ok(())
    }
}
