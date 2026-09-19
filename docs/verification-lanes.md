# Verification Lanes, Size Discipline, and Audit Contracts

FrankenRemote enforces all build, verification, size discipline, and security boundary checks through repository-owned commands (`scripts/verify.sh`), fulfilling AGENTS.md Sections 3, 10, and Plan Section 20.3, 22.2.

---

## 1. Verification Lanes Overview

| Lane | Command | Description |
|---|---|---|
| **Fast** | `./scripts/verify.sh fast` | Formats, checks, lints (`-D warnings`), and tests the entire workspace with all features. |
| **Docs** | `./scripts/verify.sh docs` | Verifies all Markdown links and presence of required constitutional documents. |
| **Per-Crate** | `./scripts/verify.sh crate <name> [action]` | Runs checks (`fast`, `check`, `clippy`, `test`, `fmt`) scoped to `crates/<name>`. |
| **Count** | `./scripts/verify.sh count [--fixture DIR]` | Reports handwritten Rust, generated bindings, non-Rust glue, vendored source, and upstream changes. Enforces line budgets. |
| **Audit** | `./scripts/verify.sh audit [--fixture DIR]` | Feature-resolved per-target dependency and memory-safety boundary (`#![forbid(unsafe_code)]`) audit. |
| **Test Fixtures** | `./scripts/verify.sh test-fixtures` | Runs planted over-budget and architectural violation fixtures proving negative detection. |
| **Full** | `./scripts/verify.sh full` | Runs fast, docs, count, audit, test-fixtures, and UBS static analysis. |
| **Release** | `./scripts/verify.sh release` | Typed refusal until signed native artifacts and qualification matrix are complete. |

---

## 2. Size Discipline Contract (`scripts/count.py`)

Per **AGENTS.md Section 3.7** and **Plan Section 22.2**:
- **Target Handwritten Rust**: 194,000 lines
- **Planned Maximum**: 240,000 lines
- **Hard Stop**: 250,000 lines (exceeding triggers `OVER_BUDGET_REFUSAL` and exits 1)
- **Non-Rust Glue Allowance**: 20,000 lines (JS/Swift/Kotlin/C/Python/shell; exceeding exits 1)

### Category Accounting
1. **Handwritten Rust**: All non-generated `.rs` files across project crates and test harnesses.
2. **Generated Bindings**: Bindgen output or files marked `@generated`, accounted separately.
3. **Non-Rust Glue**: Swift, Kotlin, JavaScript, C/C++, Python, and Shell glue.
4. **Vendored Source**: Third-party vendored code in `vendor/` (if present).
5. **Project Upstream Changes**: Patches or upstream additions in `upstream/` or `patches/`.

---

## 3. Target Dependency & Safety Audit (`scripts/audit.py`)

Per **AGENTS.md Sections 3.1, 3.2, 3.5**:
1. **Memory-Safety Boundary**:
   - Every workspace crate except the named boundary crates (`fr-native`, `fr-ffi`) must enforce `#![forbid(unsafe_code)]` at its root module (`src/lib.rs` / `src/main.rs`).
   - Zero `unsafe` code is permitted outside the named boundary crates.
2. **One Runtime**:
   - Asupersync is the sole async runtime.
   - Forbidden across all targets: `tokio`, `async-std`, `smol`, `libwebrtc`, `electron`, `chromium`, `actix`, `rocket`, `axum`.
3. **Target Constraints**:
   - **Browser Target (`wasm32-unknown-unknown`)**: Inherits no desktop FFI (`fr-native`, `fr-ffi`, X11, D3D, Metal, ScreenCaptureKit).
   - **Daemon (`frd`)**: Inherits no desktop GUI or windowing libraries (`winit`, `gtk`, `qt`, `viewer_window`).
   - **Dependency Graph Classification**: Direct, build-only, test-only, native, and transitive dependencies are reported separately.

---

## 4. Planted Negative Fixture Proofs (`scripts/test_verify_lanes.py`)

The verification lane includes automated tests with planted negative fixtures proving that drift is caught:
1. Planted over-budget Rust fixture (>250,000 lines) triggers `OVER_BUDGET_REFUSAL` (exit 1).
2. Planted over-budget glue fixture (>20,000 lines) triggers `OVER_BUDGET_REFUSAL` (exit 1).
3. Planted `unsafe` usage in a safe crate triggers `AUDIT_REFUSAL` (exit 1).
4. Planted missing `#![forbid(unsafe_code)]` triggers `AUDIT_REFUSAL` (exit 1).
5. Planted desktop FFI in a browser target triggers `AUDIT_REFUSAL` (exit 1).
6. Planted forbidden runtime (`tokio`) triggers `AUDIT_REFUSAL` (exit 1).
