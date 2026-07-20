# ADR-004: Adopt cargo-dist for desktop binary release tooling

## Status
Proposed

## Context

Graphite's desktop binary is currently packaged by a hand-rolled Rust crate,
`graphite-desktop-bundle` (`desktop/bundle/`), which is the final step of
`cargo run build desktop`:

- **macOS** (`mac.rs`): assembles `Graphite.app` (main + 3 helper apps + CEF
  framework + generated `Info.plist`). CI then deep-codesigns + notarizes.
- **Windows** (`win.rs`): copies a trimmed CEF folder → `Graphite/` (no installer).
- **Linux** (`linux.rs`): a `// TODO: not yet implemented` stub. Linux distribution
  is instead delegated to Nix (`nix build .#graphite-bundle` → `tar.xz`) and Flatpak
  in `.github/workflows/build.yml`.

This is maintenance-heavy and **Linux is incomplete in the Rust crate**. Separately,
the UI webview host (CEF) is a heavy, sandbox-unbuildable dependency — but that
concern is **orthogonal** to packaging (it is tracked in ADR-002). We wanted a
framework-agnostic, cross-platform release pipeline that emits per-OS installers,
handles signing/notarization, and uploads to GitHub Releases, without us maintaining
per-OS bundling Rust code.

## Decision

Adopt **`cargo-dist`** as the release/publish orchestrator. The runtime shell
(`winit` + `wgpu` + CEF, or a future Wry swap) is left **untouched** — cargo-dist
only packages and publishes; it is agnostic to how the binary was built.

Concretely:

- Add `dist-workspace.toml` at the workspace root declaring targets
  (`x86_64`/`aarch64`-apple-darwin, `x86_64-pc-windows-msvc`,
  `x86_64-unknown-linux-gnu`) and installers (`pkg`, `msi`, `deb`, `rpm`, `appimage`).
- Let `cargo dist init` (run on a networked Mac) pin the cargo-dist version and
  generate `.github/workflows/release.yml`, which builds release binaries, produces
  installers, and uploads to GitHub Releases on tag.
- **macOS `.app` assembly**: cargo-dist packages `cargo build --release` output and
  does not itself assemble an `.app`. The existing `desktop/bundle` crate (or a thin
  wrapper script) is invoked as a pre-packaging step in the generated workflow to
  produce the `.app`, which is then handed to cargo-dist. Whether to keep
  `desktop/bundle` long-term or replace it with cargo-dist installers is a follow-up
  to validate on a Mac.
- Distribution is scoped to the `graphite` desktop binary; `graphene-cli` and other
  workspace tooling binaries are excluded from the release.

## Consequences

**Easier / gained**
- One tool yields cross-platform installers + signing/notarization; Linux is no
  longer a Rust TODO stub.
- Less custom bundling Rust code to maintain; reproducible, version-pinned releases.
- Packaging concern is decoupled from the webview-host concern (ADR-002), so each
  can evolve independently.

**Harder / trade-offs given up**
- Adds cargo-dist as a release-time dependency and a generated workflow that must
  stay in sync with its version (config drift risk if not re-run via `cargo dist init`).
- cargo-dist packages `cargo build --release` binaries, so Graphite's OS-specific
  `.app`/bundle assembly still needs a **custom step** — cargo-dist does not replace
  that logic. The win is the *installer + upload* layer, not the app-bundle layer.
- The desktop binary **still requires CEF to build** (unchanged). Releases therefore
  still need a machine/CI where CEF downloads. cargo-dist removes the *packaging*
  burden, not the *CEF build* burden.

**Reversibility**
- Config is two files (`dist-workspace.toml` + generated `release.yml`); removing
  cargo-dist is a clean revert. No runtime code depends on it.

## Note on environment limitations (2026-07-20)
The scaffold + this ADR were authored in a sandbox where (a) CEF cannot be
downloaded, so no binary can be built, and (b) there is no authenticated GitHub
connection, so nothing can be pushed. The scaffold must be validated and the actual
release performed on a networked macOS machine with GitHub auth.
