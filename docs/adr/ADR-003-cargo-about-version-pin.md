# ADR-003: Pin cargo-about to 0.9.1 and migrate `about.toml` schema

## Status
Accepted

## Context
The desktop build chain runs `cargo run -p third-party-licenses --features desktop`, which shells out to `cargo about generate` to emit the third-party license notices bundled with the app.

Two things drifted out of sync:

1. `tools/cargo-run/src/requirements.rs` installed cargo-about with **no version pin** and **without `--features cli`**, so the latest release (0.9.1) was installed. The `cli` feature warning the user hit earlier (`bin "cargo-about" requires the features: cli`) comes directly from this missing flag.
2. Graphite's `about.toml` was authored for an older cargo-about where `no-clearly-defined = true` was a valid **boolean** whose job was to disable the ClearlyDefined network API and fall back to local license-file checking (added because that API "occasionally … return errors for at least a full day", see #1653).

cargo-about 0.9.x removed the `no-clearly-defined` boolean (it is now a per-crate `allow`/`deny` table) and moved the "disable ClearlyDefined" behavior to the **`--offline` flag**. The build failed with:

```
error: expected a table, found boolean
   ┌─ /Users/nil/Documents/Graphite/about.toml:26:22
26 │ no-clearly-defined = true
```

Note: the `[webpki.clarify]` / `[rustls-webpki.clarify]` syntax in the same file was **already correct** for 0.9.1 (the canonical cargo-about `about.toml` uses `[codespan.clarify]`); only the boolean was invalid.

## Decision
1. **Migrate `about.toml` to the 0.9.1 schema** — keep the `[*crate*.clarify]` blocks, remove the obsolete `no-clearly-defined = true` boolean, and update its comment to point at the new mechanism.
2. **Preserve the original intent** (avoid the flaky ClearlyDefined API) by passing `--offline` to `cargo about generate` in `tools/third-party-licenses/src/cargo.rs`. This is the faithful 0.9.x translation of the old boolean: local-only license checking, no `clearlydefined.io`.
3. **Pin the tool version** in `tools/cargo-run/src/requirements.rs` to `cargo install cargo-about@0.9.1 --features cli`, so the self-install is reproducible and stops warning.

## Consequences
- `cargo about generate` now runs fully offline; the build no longer depends on `clearlydefined.io` and no longer fails on the config parse error.
- **Trade-off:** with `--offline`, crates that don't ship a `LICENSE` file fall back to the default license text (may omit upstream copyright headers). This is the same limitation the old `no-clearly-defined = true` already accepted, so behavior is preserved, not degraded.
- Pinning to 0.9.1 means a future cargo-about schema change won't silently break the build again — but `about.toml` is now coupled to the 0.9.x schema and must be kept in sync if the pin is bumped.
- If richer license text is ever wanted (online), the reversible path is: drop `--offline` from `cargo.rs` and instead manage specific crates via the `[no-clearly-defined]` `allow`/`deny` table.
- Validated: `cargo about generate --format json --locked` exits 0 and emits valid JSON after the change.
