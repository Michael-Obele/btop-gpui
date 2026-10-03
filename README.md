# btop-gpui

A native Linux desktop system monitor in the spirit of
[btop](https://github.com/aristocratos/btop), written in Rust with
[GPUI](https://gpui.rs) via [gpui-kit](https://gpui-kit.com) instead of a
terminal TUI.

Numbers come from `/proc` and `/sys` directly, so they match btop side by side.
Linux only (X11 + Wayland). GPU panels are v2.

---

## Prerequisites

### Rust

```bash
rustup toolchain install stable
rustup default stable
rustc --version   # >= 1.92 required (1.97.1 verified working)
```

`rust-toolchain.toml` pins `channel = "stable"`; `Cargo.toml` asserts
`rust-version = "1.92"`. gpui-kit 0.7.0 pulls `gpui-pre = 0.3.7`.

### System libraries (Ubuntu 24.04 — verified by upstream)

```bash
sudo apt update
sudo apt install -y gcc g++ clang libfontconfig-dev libwayland-dev \
  libwebkit2gtk-4.1-dev libxkbcommon-x11-dev libx11-xcb-dev \
  libssl-dev libzstd-dev vulkan-validationlayers libvulkan1
```

`libclang-dev` is effectively mandatory: GPUI depends on `bindgen`. If the
linker names a missing library, install the package it names and re-run.

### Vulkan — the hard requirement

GPUI renders through `wgpu` on Vulkan and **rejects software adapters**. There
is no `llvmpipe` fallback in the merged code.

```bash
vulkaninfo --summary | head -40   # must list a real device
```

If `vulkaninfo` errors, stop. The app will not start and no amount of Rust will
fix it — install the driver first. `libvulkan1` alone is the loader, not a GPU
driver.

### Optional capabilities

| Feature                                          | Requirement                               | Command                                                                      |
| ------------------------------------------------ | ----------------------------------------- | ---------------------------------------------------------------------------- |
| CPU wattage (`energy_uj`)                        | `cap_perfmon`                             | `sudo setcap cap_perfmon=+ep ./target/release/btop-gpui`                     |
| `kill` / `setpriority` on other users' processes | usually fine; helps on some `/proc` reads | `sudo setcap cap_perfmon,cap_dac_read_search=+ep ./target/release/btop-gpui` |

Never ship SUID. Without caps the app still works: watts hide, `/proc/<pid>/io`
renders `—`, `kill` on a foreign process surfaces EPERM without dying.

---

## Build and run

```bash
cargo run                    # debug build + window
cargo run --release          # slow: lto = "thin", codegen-units = 1 on ~33 MB
cargo build --bin btop-gpui  # binary only, no window
```

The first build downloads well over 300 MB of crates and compiles ~850 of them.
Expect several minutes.

### CPU budget — read before you build

Builds on this machine are a metered resource. A default `cargo build` fans out
to 8 parallel `rustc` processes, pins every core, and throttles for minutes.
`target/` is already several GB.

Create `.cargo/config.toml` (it does not exist yet):

```toml
[build]
jobs = 4
```

Then pick the cheapest command that proves the change:

| Instead of                             | Run                                                 | Why                                                 |
| -------------------------------------- | --------------------------------------------------- | --------------------------------------------------- |
| `cargo build`                          | `cargo check`                                       | Type-checks without codegen. Several times cheaper. |
| `cargo build` (to check tests compile) | `cargo check --all-targets`                         | Same, plus test/example targets.                    |
| `cargo test`                           | `cargo test --lib`                                  | Avoids building the ~33 MB GPUI binary.             |
| `cargo clippy --all-targets`           | `cargo check -j 2` when the machine is already busy | Halves the thermal load.                            |

Hard rules: **one cargo command at a time** (two contend for the `target/` lock),
never `cargo clean` without asking (it discards a multi-GB warm cache), never edit
dependency versions casually, and ask before any `--release` build.

---

## The full verification set

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --lib
cargo check --all-targets
```

`cargo test --lib` must report **126 passed / 0 failed**. A test command that
discovers zero tests exits 0 — treat "0 tests run" as a failure, not a pass.

Run these at the end of a task, not in a loop.

---

## Rust equivalents of the `bun` script set

There is no single all-in-one checker in Rust. `bun check` is three tools
running together; the equivalent is the four commands above.

| `bun`                     | Rust                                        | Notes                                                              |
| ------------------------- | ------------------------------------------- | ------------------------------------------------------------------ |
| `bun install`             | `cargo fetch`                               | Or let any build do it.                                            |
| `bun check`               | `cargo check --all-targets`                 | Type-check, no codegen. The closest single equivalent.             |
| `bun run lint`            | `cargo clippy --all-targets -- -D warnings` | Lint. `-D warnings` makes warnings fatal, as CI wants.             |
| `bun run format`          | `cargo fmt --all`                           | Rewrites. `--check` to verify without writing.                     |
| `bun test`                | `cargo test --lib`                          | `--lib` avoids the binary; drop it to include integration targets. |
| `bun test <name>`         | `cargo test --lib <name>`                   | Filters by substring.                                              |
| `bun run build`           | `cargo build --release`                     | Expensive. Ask first.                                              |
| `bun run dev`             | `cargo run`                                 |                                                                    |
| `bun run start`           | `./target/release/btop-gpui`                |                                                                    |
| TypeScript `tsc --noEmit` | `cargo check --all-targets`                 | Rust has no separate type-check phase; it is part of codegen.      |

To make `bun check` work verbatim, add a `.cargo/config.toml` alias:

```toml
[alias]
check = "check --all-targets"
lint = "clippy --all-targets -- -D warnings"
fmtcheck = "fmt --all --check"
t = "test --lib"
```

Then `cargo check`, `cargo lint`, `cargo fmtcheck`, `cargo t`.

---

## Testing

Tests are in-crate `#[cfg(test)] mod tests` next to the code they test. There is
no integration test binary; `tests/` holds only `fixtures/`.

```bash
cargo test --lib                      # whole suite, no GPUI binary built
cargo test --lib parse_pid_stat       # one test while iterating
cargo test --lib -- --nocapture       # see println! output
```

Rules:

- **Never read the live machine and assert a value.** Fixtures only, loaded from
  `tests/fixtures/` via `CARGO_MANIFEST_DIR`.
- The fixtures cover the traps deliberately: a `comm` containing a space and a
  parenthesis, a `stat` block, a `meminfo`, a `cpuinfo`, a `filesystems`, a
  `self/mounts`.
- A fixture-based test must **fail loudly on a missing fixture**. A test that
  prints and returns is green with zero assertions.

---

## Layout

```
src/
  main.rs        boot: panic hook -> config -> gpui_kit::init -> collector thread -> window
  lib.rs         module list only
  app.rs         AppView: owns UI state, the tick loop, the pull()/render() split
  model.rs       Snapshot + per-metric structs. Plain data, no behaviour.
  history.rs     Ring<T>: fixed-capacity ring, capacity = graph_width * 2
  format.rs      bytes / rate / temperature / frequency / duration / percent -> String
  config.rs      Config: load, save, defaults, live-apply
  logger.rs      log file + log-once set
  collect/       sysfs cpu temp mem disk net proc battery gpu(stub)
  ui/            actions chart chrome dialogs panels theme themes
tests/fixtures/  captured /proc and /sys files for the parser tests
docs/            gitignored working material, not part of the repo
```

### The one structural rule

`src/collect/**`, `model.rs`, `format.rs`, `history.rs`, `config.rs` and
`logger.rs` are the **pure data layer**: they never import GPUI, never touch a
window, and read only `/proc` and `/sys`. `src/app.rs`, `src/ui/**` and
`src/main.rs` are the only GPUI-aware code, and they only ever read a snapshot
a collector already produced.

That split is what makes `cargo test` meaningful on a machine with no display and
no GPU. Verify it after any refactor — both must print nothing:

```bash
grep -rn "gpui" src/collect/ src/model.rs
grep -n "notify()" src/app.rs    # none may sit inside render()
```

---

## Runtime files

| What          | Where                                                                  |
| ------------- | ---------------------------------------------------------------------- |
| Config        | `$XDG_CONFIG_HOME/btop-gpui/btop-gpui.conf` (default `~/.config/…`)    |
| Custom themes | `$XDG_DATA_HOME/btop-gpui/themes` (default `~/.local/share/…`)         |
| Log           | `$XDG_STATE_HOME/btop-gpui/btop-gpui.log` (default `~/.local/state/…`) |

Delete the config and relaunch: defaults are written back with a comment block.
Corrupt a value and it falls back to the default, logs once, and starts.

Useful keys in the config: `update_ms` (tick interval), `base_10_sizes`,
`proc_per_core`, `disks_filter`, `proc_filter`, `update_interval`.

---

## Keyboard

| Key             | Action                       |
| --------------- | ---------------------------- |
| `?`             | Toggle help                  |
| `Esc`           | Dismiss dialog / overlay     |
| `p` / `Shift-p` | Next / previous panel preset |
| `Shift-t`       | Toggle process tree          |
| `c`             | Cycle sort column            |
| `r`             | Reverse sort direction       |

---

## Current state

Verified on 2026-10-01 with rustc 1.97.1:

| Command                                     | Result                                                      |
| ------------------------------------------- | ----------------------------------------------------------- |
| `cargo test --lib`                          | **126 passed, 0 failed**                                    |
| `cargo clippy --all-targets -- -D warnings` | clean                                                       |
| `cargo check --all-targets`                 | clean                                                       |
| `cargo fmt --all --check`                   | **fails** — `src/app.rs` import order and a let-chain brace |

Not yet built: any `--release` artifact, and the Vulkan/window path is unverified
on this machine (`vulkaninfo` was not exercised here).

---

## Conventions worth knowing

- **Every read is fallible.** A missing `/sys` file, a vanished PID, a permission
  error degrades to `None` and renders `—`. No `unwrap()` or `expect()` outside
  `#[cfg(test)]` on any path touching `/proc`, `/sys`, signals or config. The only
  exceptions are the two startup calls in `main.rs`.
- **Read a pseudo-file once per tick**, diff against the previous sample, cache.
  A second read in the same tick returns a different value and makes graphs jitter.
- **`Instant`, never wall-clock time.** NTP steps produce negative deltas.
- **Units are fixed at the model boundary.** Store raw units, convert only in
  `format.rs`. Field-name suffixes are mandatory: `_bytes`, `_bytes_per_sec`,
  `mhz`, `temp_c`, `_percent`, `_ticks`. `/proc/meminfo` is kB (`<< 10`), disk
  sectors are always 512 B, sysfs temps are milli-°C (`/1000.0`),
  `scaling_cur_freq` is kHz.
- **`cx.notify()` belongs in the tick, never in `render()`.**
- **`pull()` allocates; `render()` does not.** Sorting, filtering and folding
  history all happen once per tick in `pull()`.

Full rules live in `AGENTS.md`. Read it before your first edit — it is the
working agreement for this repo and for AI agents working in it.
