//! Copies the static frontend from `apps/ui` into `apps/legion-app/dist` before Tauri embeds it.
//!
//! `apps/ui/design` and `apps/ui/test` are skipped. Until `apps/ui/index.html` exists, a small
//! built-in placeholder page is written into `dist/` so the shell can be built and verified on its
//! own.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const SKIPPED_TOP_LEVEL: &[&str] = &["design", "test"];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let ui_dir = manifest_dir.join("../../ui");
    let dist_dir = manifest_dir.join("../dist");
    let root_cargo = manifest_dir.join("../../../Cargo.toml");

    println!("cargo:rerun-if-changed={}", ui_dir.display());
    println!("cargo:rerun-if-changed={}", root_cargo.display());

    check_workspace_version(&root_cargo);

    stage_frontend(&ui_dir, &dist_dir).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to stage frontend from {} to {}: {err}",
                ui_dir.display(),
                dist_dir.display()
            ),
        )
    })?;

    tauri_build::build();
    Ok(())
}

/// Ensures the standalone app crate keeps the same version as the root workspace.
fn check_workspace_version(root_cargo: &Path) {
    let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let Ok(contents) = fs::read_to_string(root_cargo) else {
        return;
    };

    let mut in_workspace_package = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_workspace_package = trimmed == "[workspace.package]";
            continue;
        }
        if in_workspace_package && let Some(rest) = trimmed.strip_prefix("version") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let value = rest.trim().trim_matches('"');
                if !value.is_empty() && value != pkg_version {
                    println!(
                        "cargo:warning=legion-app version ({pkg_version}) differs from root workspace version ({value})"
                    );
                }
                break;
            }
        }
    }
}

fn stage_frontend(ui_dir: &Path, dist_dir: &Path) -> io::Result<()> {
    if dist_dir.exists() {
        fs::remove_dir_all(dist_dir)?;
    }
    fs::create_dir_all(dist_dir)?;

    if ui_dir.is_dir() {
        copy_ui_tree(ui_dir, ui_dir, dist_dir)?;
    } else {
        println!(
            "cargo:warning=apps/ui directory not found at {}; bundling built-in placeholder",
            ui_dir.display()
        );
    }

    if !dist_dir.join("index.html").is_file() {
        println!(
            "cargo:warning=apps/ui/index.html is not present yet; bundling built-in placeholder UI"
        );
        fs::write(dist_dir.join("index.html"), PLACEHOLDER_HTML)?;
        fs::write(dist_dir.join("placeholder.css"), PLACEHOLDER_CSS)?;
        fs::write(dist_dir.join("placeholder.js"), PLACEHOLDER_JS)?;
    }

    Ok(())
}

fn copy_ui_tree(root: &Path, src: &Path, dst: &Path) -> io::Result<()> {
    let mut entries = fs::read_dir(src)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        if src == root && SKIPPED_TOP_LEVEL.contains(&name.as_ref()) {
            continue;
        }
        if name.starts_with('.') {
            continue;
        }

        println!("cargo:rerun-if-changed={}", path.display());

        let file_type = entry.file_type()?;
        let target = dst.join(&file_name);
        if file_type.is_dir() {
            fs::create_dir_all(&target)?;
            copy_ui_tree(root, &path, &target)?;
        } else if file_type.is_file() {
            fs::copy(&path, &target)?;
        }
    }

    Ok(())
}

const PLACEHOLDER_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
  <title>Legion</title>
  <link rel="stylesheet" href="styles.css">
  <link rel="stylesheet" href="placeholder.css">
</head>
<body>
  <div class="inset-probe" aria-hidden="true"></div>
  <div class="shell">
    <div class="app">
      <header class="bar">
        <button id="back-btn" class="btn icon back hidden" type="button" aria-label="Back">&#8592;</button>
        <h1 class="wordmark" id="bar-title"><b>&#9613;</b>Legion</h1>
        <span class="chip" id="status-chip">unpaired</span>
      </header>

      <main class="main" id="screen-home">
        <p class="label">Shell status</p>
        <div class="list">
          <dl class="kv">
            <dt>device</dt>
            <dd id="device-name">loading…</dd>
            <dt>ready</dt>
            <dd id="state-ready">…</dd>
            <dt>needs</dt>
            <dd id="state-needs">…</dd>
            <dt>safe insets</dt>
            <dd id="safe-insets">…</dd>
          </dl>
        </div>

        <p class="label">Backend response</p>
        <pre class="out wrap" id="state-json">Loading…</pre>

        <div class="actions">
          <button id="open-detail" class="btn primary" type="button">Inspect shell</button>
          <button id="probe-tools" class="btn" type="button">Test tools call</button>
        </div>

        <pre class="out wrap hidden" id="tools-result"></pre>
      </main>

      <main class="main hidden" id="screen-detail">
        <p class="label">Second screen</p>
        <div class="notice">
          <strong>Navigation check</strong>
          <span>Use the back button or the system back gesture to return to the home screen.</span>
        </div>
        <div class="list">
          <dl class="kv">
            <dt>route</dt>
            <dd id="route-name">#detail</dd>
            <dt>history</dt>
            <dd id="history-len">…</dd>
          </dl>
        </div>
      </main>

      <nav class="nav" aria-label="Primary">
        <a href="#home" id="nav-home" aria-current="page">Home</a>
        <a href="#detail" id="nav-detail">Shell</a>
      </nav>
    </div>
  </div>
  <script src="placeholder.js"></script>
</body>
</html>
"##;

const PLACEHOLDER_CSS: &str = r##".hidden {
  display: none !important;
}

/* Measure raw CSS env(safe-area-inset-*) directly, separate from activity CSS variables. */
.inset-probe {
  position: fixed;
  top: 0;
  left: 0;
  width: 0;
  height: 0;
  visibility: hidden;
  padding-top: env(safe-area-inset-top, 0px);
  padding-right: env(safe-area-inset-right, 0px);
  padding-bottom: env(safe-area-inset-bottom, 0px);
  padding-left: env(safe-area-inset-left, 0px);
}

.bar {
  padding-top: calc(max(env(safe-area-inset-top, 0px), var(--safe-area-inset-top, 0px)) + 6px);
  padding-left: calc(var(--pad) + max(env(safe-area-inset-left, 0px), var(--safe-area-inset-left, 0px)));
  padding-right: calc(var(--pad) + max(env(safe-area-inset-right, 0px), var(--safe-area-inset-right, 0px)));
}

.nav {
  padding-bottom: max(env(safe-area-inset-bottom, 0px), var(--safe-area-inset-bottom, 0px));
}
"##;

const PLACEHOLDER_JS: &str = r##"(function () {
  "use strict";

  const invoke = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;

  const backBtn = document.getElementById("back-btn");
  const statusChip = document.getElementById("status-chip");
  const deviceEl = document.getElementById("device-name");
  const readyEl = document.getElementById("state-ready");
  const needsEl = document.getElementById("state-needs");
  const insetsEl = document.getElementById("safe-insets");
  const stateJsonEl = document.getElementById("state-json");
  const toolsResultEl = document.getElementById("tools-result");
  const screenHome = document.getElementById("screen-home");
  const screenDetail = document.getElementById("screen-detail");
  const historyLenEl = document.getElementById("history-len");
  const navHome = document.getElementById("nav-home");
  const navDetail = document.getElementById("nav-detail");
  const openDetailBtn = document.getElementById("open-detail");
  const probeToolsBtn = document.getElementById("probe-tools");

  function renderRoute() {
    const isDetail = window.location.hash === "#detail";
    screenHome.classList.toggle("hidden", isDetail);
    screenDetail.classList.toggle("hidden", !isDetail);
    backBtn.classList.toggle("hidden", !isDetail);
    navHome.setAttribute("aria-current", isDetail ? "false" : "page");
    navDetail.setAttribute("aria-current", isDetail ? "page" : "false");
    historyLenEl.textContent = String(window.history.length);
  }

  function updateInsets() {
    const probe = document.querySelector(".inset-probe");
    if (!probe) return;
    const cs = window.getComputedStyle(probe);
    const rootCs = window.getComputedStyle(document.documentElement);
    const actTop = rootCs.getPropertyValue("--safe-area-inset-top").trim() || "0px";
    const actBot = rootCs.getPropertyValue("--safe-area-inset-bottom").trim() || "0px";
    insetsEl.textContent =
      "env top " + cs.paddingTop + " / bottom " + cs.paddingBottom +
      " · activity top " + actTop + " / bottom " + actBot;
  }

  window.addEventListener("popstate", renderRoute);
  window.addEventListener("hashchange", renderRoute);
  window.addEventListener("resize", updateInsets);
  window.addEventListener("legion-insets", updateInsets);

  openDetailBtn.addEventListener("click", function () {
    if (window.location.hash !== "#detail") {
      window.history.pushState({ screen: "detail" }, "", "#detail");
      renderRoute();
    }
  });

  backBtn.addEventListener("click", function () {
    window.history.back();
  });

  probeToolsBtn.addEventListener("click", async function () {
    toolsResultEl.classList.remove("hidden");
    if (!invoke) {
      toolsResultEl.textContent = "Tauri IPC is unavailable.";
      return;
    }
    try {
      const res = await invoke("tools", { on: null });
      toolsResultEl.textContent = JSON.stringify(res, null, 2);
    } catch (err) {
      toolsResultEl.textContent = JSON.stringify(err, null, 2);
    }
  });

  async function init() {
    renderRoute();
    updateInsets();
    window.setTimeout(updateInsets, 250);

    if (!invoke) {
      stateJsonEl.textContent = "Tauri IPC not available in this context.";
      return;
    }

    try {
      const [name, st] = await Promise.all([
        invoke("device_name"),
        invoke("state"),
      ]);
      deviceEl.textContent = String(name);
      readyEl.textContent = String(st.ready);
      needsEl.textContent = st.needs === null ? "none" : String(st.needs);
      statusChip.textContent = st.ready ? "ready" : (st.needs || "unpaired");
      stateJsonEl.textContent = JSON.stringify(st, null, 2);
    } catch (err) {
      statusChip.textContent = "error";
      stateJsonEl.textContent = JSON.stringify(err, null, 2);
    }
  }

  init();
})();
"##;
