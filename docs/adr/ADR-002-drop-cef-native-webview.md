# ADR-002: Drop CEF → OS-native webview for the desktop shell

## Status
Proposed

## Context

The desktop shell (ADR-001) is `winit` + `wgpu` + **CEF (Chromium)**, where CEF hosts the existing Svelte/TS frontend and composites the native `wgpu` document canvas underneath it via zero-copy platform surfaces (`dmabuf`/`iosurface`/`d3d11`).

Attempting to build/run the shell in this environment failed at two independent points:

1. **Native WASM wrapper not built.** The `cargo-run` `build_wasm(release, native=true)` step runs `wasm-bindgen` and `wasm-opt` as external CLIs. They are not installed (`which wasm-bindgen` / `which wasm-opt` → missing; `frontend/wrapper/pkg-native` absent). cargo-run fails with `sequence: failed to start step: No such file or directory` (ENOENT on `wasm-bindgen`), so `pkg-native/graphite_wasm_wrapper.{js,wasm}` is never produced and Vite then fails with `UNLOADABLE_DEPENDENCY … /wrapper/pkg-native/graphite_wasm_wrapper`. Pinned version is `wasm-bindgen = "=0.2.121"` (must match `wasm-bindgen-cli`). The `wasm32-unknown-unknown` target IS installed.
2. **CEF binaries unreachable.** `cef-build-storage.googleapis.com` returned `000` (unreachable). The `cef` crate downloads a Chromium distribution at build time, so the `graphite-desktop-ui` (CEF) crate cannot compile here.

Consequence: the **original CEF plan cannot run in this environment** — fixing blocker #1 only advances the build to blocker #2. Moreover, blocker #1 is independent of the UI host: the frontend JS imports `graphite_wasm_wrapper` to talk to the editor via `graphite_wasm_wrapper::native_communication`, so the WASM wrapper must build regardless of whether the host is CEF or a native webview.

**Additional constraint (verified in code):** the App composites *three* wgpu textures onto one window — document canvas (from the editor's node graph), vello overlays, and the **UI texture supplied by the UI host** via `RenderState::bind_ui_texture` / `UiEvent::Frame(texture)` (`desktop/src/render/state.rs`, `composite_shader.wgsl`). CEF is used specifically because it can export its rendered web UI as a GPU texture into that UI slot (zero-copy via `dmabuf`/`iosurface`/`d3d11`). So dropping CEF is **not** a drop-in UI-host swap: it forces re-architecting compositing to the **B2 two-window model** (a wgpu canvas window + a transparent Wry UI window overlaid on top), which touches `desktop/src/app.rs` and `desktop/src/render/state.rs`, not only `desktop/ui`.

## Decision

To obtain a runnable desktop shell, **remove the CEF dependency and host the existing frontend in the OS-native webview** (Wry / WebView2 on Windows, WKWebView on macOS, WebKitGTK on Linux). Keep `winit`, `wgpu`, the `DesktopWrapper` editor bridge, and the `native_communication` byte codec unchanged.

Canvas strategy (recommended default): **B1 first** — render the document to a `wgpu` texture, read it back and blit into a webview `<canvas>` each frame. Migrate to **B2** (native `wgpu` view behind a transparent webview) only if canvas framerate demands it.

Prerequisite (applies to any path): install `wasm-bindgen-cli@0.2.121` and `wasm-opt` (binaryen) on PATH, matching the workspace pin.

## Consequences

### Becomes possible / easier
- Build no longer depends on the unreachable Chromium download — the shell can compile and launch in restricted/offline environments.
- Sheds ~300 MB (Chromium payload) and the forked `cef-rs` (`graphite-149` branch) maintenance burden; on macOS, WKWebView is part of the OS.
- Same frontend + same `native_communication` codec → web/desktop parity and the shared UI contract are preserved (the core reason egui was rejected in ADR-001 still holds).

### Becomes harder / trade-offs accepted
- **Canvas compositing rework.** CEF's zero-copy surface import (`frames/import/{dmabuf,iosurface,d3d11}.rs`) is lost. B1 adds a per-frame readback/copy (simpler, may strain at high FPS); B2 needs transparency + input routing (harder, but keeps GPU canvas).
- **Engine divergence.** WebKit ≠ Chromium: subtle frontend rendering diffs possible; lose Chromium devtools.
- **WebView2 runtime** required on Windows (ships with Win11; downloadable otherwise).
- **Still headless-incompatible.** Even after this swap, a sandbox with no display server cannot show a window — only build + launch-start can be verified here.
- **Blocker #1 remains a prerequisite** — `wasm-bindgen-cli`/`wasm-opt` must be installed for *any* desktop build.
- **B2 re-architecture scope (non-trivial).** Removing the UI-texture compositing slot means changing `app.rs` (stop binding the UI texture; the Wry window overlays instead) and `render/state.rs` (drop the `ui_texture` bind-group entry / make it a transparent no-op). The hard remaining problem is **input routing for the transparent overlay** — clicks on transparent (canvas) areas must pass through to the canvas window, which needs DOM hit-testing + platform click-through. This is the real engineering cost of dropping CEF.

## Alternatives considered
| Option | Verdict | Why |
|--------|---------|-----|
| Keep CEF, fix only wasm toolchain | Rejected here | Still hits unreachable CEF download; cannot run in this environment |
| CEF → Wry/WebView2 (B1 then B2) | **Proposed** | Removes Chromium dep; keeps frontend + codec; only canvas path reworked |
| egui / native Rust UI | Rejected (ADR-001) | Full UI rewrite; kills web parity |
| Electron | Not chosen | Heavier than native webview; no native `wgpu` canvas |

## Supersedes / relates
- Supersedes the CEF-based UI host described in ADR-001 (ADR-001's winit+wgpu+shared-codec reasoning still stands; only the UI host changes).
