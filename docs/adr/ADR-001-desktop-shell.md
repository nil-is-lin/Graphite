# ADR-001: Desktop Shell — winit + wgpu + CEF, egui rejected

## Status
Accepted

## Context

Graphite ships one editor core (`editor/`, Rust) and one frontend (`frontend/`, Svelte/TypeScript). The editor compiles to WASM for the browser and runs **natively** on the desktop. The frontend is web tech (HTML/CSS/JS) and is the single source of truth for all UI: toolbars, panels, dialogs, menus, the node-graph UI.

The question was how to package the native editor into a desktop app, and specifically whether to adopt a pure-Rust GUI toolkit such as **egui** instead of the current approach.

What the current `desktop/` crate already does (verified in code):

- **winit** owns the native window + event loop (`desktop/src/lib.rs`, `desktop/Cargo.toml`). This stays.
- **wgpu** owns the GPU canvas — the editor renders the document to a `wgpu::Texture` (`desktop/src/gpu_context.rs`, `desktop/src/render.rs`). This stays.
- **CEF (Chromium Embedded Framework, `cef = "149"`)** hosts the *existing* web frontend inside that native window (`desktop/ui/`, CEF process handlers, remote host process). The web UI composites on top of the native wgpu canvas via a platform frame-import pipeline (`frames/import/{dmabuf,iosurface,d3d11}.rs`).
- The editor↔frontend contract is **one shared message codec**: `graphite_wasm_wrapper::native_communication` is used by both the WASM/web path and the native desktop wrapper (`desktop/wrapper/src/lib.rs` → `deserialize_editor_message` / `serialize_frontend_messages`). The desktop wrapper (`DesktopWrapper`) runs the editor natively and reuses the exact same byte protocol the browser uses.

So the desktop and web targets already share: the editor core, the frontend, **and** the message protocol. CEF is what lets the web frontend run unchanged on the desktop.

## Decision

Keep the existing desktop shell:

- **winit** for windowing and the event loop.
- **wgpu** for the document GPU canvas (native rendering, not WASM).
- **CEF** to host the existing Svelte/TypeScript frontend unchanged.
- **egui is explicitly rejected** as the desktop UI toolkit.

## Consequences

### What this buys us (and why it's the right call *for Graphite*)
- **UI parity for free.** The entire frontend — panels, menus, dialogs, node editor — is reused verbatim. Adopting egui would mean re-implementing all of that UI in Rust immediate mode and maintaining a second UI forever. For a design tool where the UI *is* the product, that is a massive, permanent tax.
- **One protocol, two targets.** Because the web and desktop wrappers serialize through the same `native_communication` codec, a frontend feature works on both platforms the moment it lands. egui would fork that contract and double the surface area for bugs.
- **Native GPU canvas with web overlay.** wgpu renders the document at native speed; CEF's web UI is composited over it via zero-copy platform surfaces (dmabuf/iOSurface/D3D11). This is the best of both: fast canvas, rich UI. egui *could* render the canvas too, but then you lose the web frontend entirely.
- **Team skills match the stack.** The frontend is already a Svelte/TS codebase; nobody has to become an egui layout expert.

### What we are accepting (name the trade-off)
- **Binary size & footprint.** CEF pulls in a Chromium bundle (~hundreds of MB, plus a forked `cef-rs` on the `graphite-149` branch). egui would yield a dramatically smaller executable. This is the real cost of the decision — accepted because UI parity outweighs download size for this product.
- **CEF maintenance burden.** Chromium updates, the custom fork, and the multi-process/IPC plumbing (`remote::host`, helper processes) are non-trivial to keep current. "What happens when CEF 150 ships?" is a standing to-do, not a solved problem.
- **Startup complexity.** The shell spawns helper processes and runs a handshake (`UiSetupResult::Helper`); failure modes are more varied than a single-process egui app.

### Reversibility
High. The editor core and frontend are untouched; only the `desktop/` shell is involved. If CEF ever becomes unmaintainable, the swap target would be a lighter webview (e.g. Wry/WebView2) that still hosts the same frontend — *not* egui, because that would reintroduce the UI-rewrite tax we are deliberately avoiding.

## Alternatives considered
| Option | Verdict | Why |
|--------|---------|-----|
| winit + wgpu + CEF (status quo) | **Accepted** | Reuses frontend + shared codec; native GPU canvas |
| winit + wgpu + egui | Rejected | Forces a full Rust UI rewrite; kills web parity; permanent second UI to maintain |
| Electron (web frontend only) | Not chosen | No native wgpu canvas; editor would stay WASM; heavier and slower than current native core |
| winit + wgpu + Wry/WebView2 | Deferred | Lighter than CEF, still hosts the same frontend; viable future swap if CEF maintenance bites |
