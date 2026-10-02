# AGENTS.md

Working agreement for AI coding agents (and humans) in this repo. Read this
before your first edit. **This file is the single source of truth for every
agent** — there is no `CLAUDE.md` and no `.github/copilot-instructions.md`, and
there should be no second copy to keep in sync.

**btop-gpui** is a native Linux desktop system monitor in the spirit of
[btop](https://github.com/aristocratos/btop), written in Rust with
[GPUI](https://gpui.rs) + [gpui-kit](https://gpui-kit.com) instead of a
terminal TUI. Linux only (X11 + Wayland). Full btop parity is the v1 scope;
GPU panels are v2.

---

## 1. The one structural rule: two layers, one direction

| Layer               | Modules                                                                                               | Rule                                                                                                                                                  |
| ------------------- | ----------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Pure data layer** | `src/collect/**`, `src/model.rs`, `src/format.rs`, `src/history.rs`, `src/config.rs`, `src/logger.rs` | **Never import GPUI.** Never touch a window. Reads `/proc` and `/sys` only. Fully testable headless.                                                  |
| **UI layer**        | `src/app.rs`, `src/ui/**`, `src/main.rs`                                                              | The only GPUI-aware code. It only ever **reads** a snapshot a collector already produced: no `/proc` access, no sorting, no allocation in `render()`. |

This split is what makes `cargo test` meaningful on a machine with no display
and no GPU. If a change makes `src/collect/` or `model.rs` depend on GPUI, the
change is wrong — restructure it instead.

Verify the invariant after any refactor (both must print nothing):

```bash
grep -rn "gpui" src/collect/ src/model.rs   # must print nothing
grep -n "notify()" src/app.rs               # review: none may sit inside render()
```

---

## 2. Commands — and the CPU budget (READ THIS FIRST)

**Builds here are expensive and they will hurt.** This is a 15 W laptop CPU
(i7-1185G7: 4 cores / 8 threads). A default `cargo build` fans out to 8
parallel `rustc` processes, pins every core, and thermally throttles the
machine for minutes. `target/` is already ~4.9 GB. Treat every compile as a
metered resource, exactly like a download.

### Use the cheapest command that can prove the change

| Instead of                                  | Run                                                   | Why                                                                                               |
| ------------------------------------------- | ----------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| `cargo build`                               | **`cargo check`**                                     | Type-checks without codegen. Several times cheaper, and catches 90 % of what you actually broke.  |
| `cargo build` (to check tests compile)      | **`cargo check --all-targets`**                       | Same, and covers test/example targets.                                                            |
| `cargo test`                                | **`cargo test --lib`**                                | Builds only the library test harness. Plain `cargo test` also builds the ~100 MB GPUI binary.     |
| `cargo test --all`                          | `cargo test --lib <name>`                             | Run the one test you touched while iterating.                                                     |
| `cargo clippy --all-targets -- -D warnings` | same, but only when you actually changed lint surface | It is a full check-mode build of everything.                                                      |
| `cargo build --release`                     | **ask first**                                         | `lto = "thin"` + `codegen-units = 1` on a 100 MB binary = many minutes of all-core burn.          |
| `cargo clean`                               | **never**                                             | Throws away a 4.9 GB warm cache and forces a multi-minute full rebuild of ~850 crates. Ask first. |

### Hard rules

- **Never run a build, release build, or `cargo clean` on your own initiative
  if it is not needed to verify the change you just made.** Ask first — the
  owner's machine is the thing paying for it.
- **One cargo command at a time.** Two concurrent invocations contend for the
  `target/` lock and double the thermal load for zero throughput gain.
- **Limit parallelism.** `.cargo/config.toml` should contain:
  `[build]` / `jobs = 4`. Four jobs keeps wall-clock time close to eight on
  this CPU while leaving the machine responsive. Override ad hoc with
  `cargo check -j 2` when the machine is already busy.
- **Do not set `build.incremental = true`** — it would override the release
  profile's `incremental = false` and make release builds slower and bigger.
- **Never edit `Cargo.toml` dependency versions or run `cargo update`** except
  as a deliberate, isolated upgrade (see §3.3). A dependency-graph change
  invalidates the whole cache.

### The full verification command set (run at the end of a task, not in a loop)

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --lib                     # must report >= 100 tests; see §7
cargo check --all-targets            # proves the binary still compiles
```

---

## 3. Golden rules

### 3.1 Never invent an API — the #1 failure mode

GPUI and gpui-kit are pre-1.0 and break weekly. Before writing any call, prove
the signature exists:

1. `https://gpui-kit.com/component/<name>.md` — per-component API (e.g.
   `chart.md`, `data-table.md`); full index at `https://gpui-kit.com/llms.txt`.
2. `https://gpui-kit.com/docs/<topic>.md` — `coding-guides.md`, `action.md`,
   `keybinding.md`, `element.md`, `entity.md`, `test.md`, `style.md`, `theme`.
3. **The vendored source** — authoritative, already on disk:
   `~/.cargo/registry/src/*/gpui-kit-0.7.0/`, `gpui-component-0.7.0/`,
   `gpui-base-0.7.0/`, `gpui-pre-0.3.7/`.

If you cannot point at a doc page or a source line, do not write the call. A
React/CSS/older-GPUI example translated by analogy is _exactly_ how you get a
plausible method that does not exist.

> `grep_search` in this VS Code setup is workspace-scoped and silently returns
> empty for paths under `~/.cargo`. Use `read_file` / `list_dir` for the
> vendored crates.

### 3.2 One UI dependency, pinned exactly

`gpui-kit = "=0.7.0"` + a committed `Cargo.lock`. It re-exports GPUI, so
`use gpui_kit::*;` gives you `Application`, `div`, `px`, `Entity`, `Context`.
**Never add `gpui`, `gpui-pre`, `gpui_platform`, or a `git =` Zed dependency.**
crates.io `gpui` (0.2.2) is frozen and does not compile; `gpui_platform` does
not exist.

### 3.3 Upgrade deliberately, never casually

A caret range on `gpui-kit` can move `gpui-pre` onto a snapshot the component
library was not built against. To upgrade: bump `gpui-kit`, let cargo move
`gpui-pre` in the same change, fix every compile error in one commit. Never
upgrade "just the platform crate". Never edit `Cargo.lock` by hand.

### 3.4 Read a pseudo-file once per tick

Read once, diff against the previous sample, cache. This is how btop stays
cheap _and_ how percentages become correct — a second read in the same tick
gives a different value and makes graphs jitter.

### 3.5 `Instant`, never wall-clock time

NTP steps produce negative or absurd deltas. `CLOCK_MONOTONIC` / `Instant` only.

### 3.6 Every read is fallible

A missing `/sys` file, a vanished PID, a permission error: degrade to `None`
and render `—`. Panics on the UI thread kill the window.

- **No `unwrap()` / `expect()` outside `#[cfg(test)]`** on any path touching
  `/proc`, `/sys`, process signals, or config parsing. Use `ok()?`,
  `unwrap_or_default()`, or an explicit `Option`.
- The only allowed exceptions are the two "this cannot fail, and if it does
  nothing downstream matters" startup calls in `main.rs`
  (`open_window(..).expect(..)`).
- A panic inside one collector tick is caught (`catch_unwind` in
  `collect::spawn`) and logged; the loop continues. **This is why
  `Cargo.toml` deliberately does not set `panic = "abort"`.** Do not add it.

### 3.7 Units are fixed at the model boundary

Store raw units; convert only in `format.rs` at render time. Field-name suffixes
are mandatory — they are how you avoid mixing kB with bytes ten edits later.

| Quantity                | Unit in the model | suffix           | Conversion                          |
| ----------------------- | ----------------- | ---------------- | ----------------------------------- |
| Memory, sizes, counters | bytes (u64)       | `_bytes`         | `/proc/meminfo` is **kB** → `<< 10` |
| Disk I/O sectors        | bytes (u64)       | —                | sectors `* 512` (always 512)        |
| Throughput              | bytes/sec (f64)   | `_bytes_per_sec` | —                                   |
| CPU frequency           | MHz (u32)         | `mhz`            | `scaling_cur_freq` is **kHz**       |
| Temperature             | °C (f32)          | `temp_c`         | sysfs is **milli-°C** → `/1000.0`   |
| Power                   | milliwatts (f32)  | —                | `energy_uj` delta → W → `*1000`     |
| Percentages             | 0.0–100.0 (f32)   | `_percent`       | never 0.0–1.0                       |
| CPU ticks               | raw jiffies (u64) | `_ticks`         | `sysconf(_SC_CLK_TCK)`, default 100 |

Never store a formatted `String` in the model. Never put UI state (sort mode,
filter text, tree state, selection, preset) in the model — it belongs to
`AppView`.

### 3.8 One small commit per task, with a conventional message

History is the debugging tool. Use `feat:` / `fix:` / `chore:` /
`docs:` + a lowercase imperative subject (`feat: cpu collector (stat, freq,
temps, watts)`). Do not bundle an unrelated refactor into a feature commit.

### 3.9 Leave the tree green

Never end a task with a broken build you "will fix next". If a change cannot be
finished, revert it rather than committing half of it.

---

## 4. Layout — where things go

```
src/
  main.rs        boot: panic hook → config → gpui_kit::init → collector thread → window
  lib.rs         module list only
  app.rs         AppView: owns all UI state, the tick loop, pull()/render() split
  model.rs       Snapshot + per-metric structs. Plain data, no behaviour.
  history.rs     Ring<T>: fixed-capacity ring, capacity = graph_width * 2
  format.rs      bytes / rate / temperature / frequency / duration / percent → String
  config.rs      Config: load, save, defaults, live-apply
  logger.rs      log file + log-once set (a missing file must not spam the log)
  collect/
    mod.rs       Collector facade, Shared cell, spawn(), catch_unwind tick
    sysfs.rs     read_str / read_u64 / read_i64 / read_lines helpers
    cpu.rs temp.rs mem.rs disk.rs net.rs proc.rs battery.rs gpu.rs(stub)
  ui/
    mod.rs  theme.rs  actions.rs  chrome.rs  dialogs.rs  chart.rs  panels/
tests/fixtures/  captured /proc and /sys files for the parser tests
```

- `gpu.rs` is a **v2 stub**: a trait plus an empty `Vec`. Do not build a GPU
  panel before v2 is opened.
- Collectors share no trait on purpose. One platform, one implementation — a
  trait with a single impl is speculative generality. Extract it when macOS
  actually arrives.
- `Shared` (`Arc<Mutex<..>>` + monotonic counter) is the only channel between
  the threads. The collector thread never touches a GPUI type; the UI thread
  never does blocking I/O.

---

## 5. Rust conventions

- **Naming** (from gpui-kit's coding guide — the same vocabulary applies to our
  own types, so keep it consistent): views/entities are product concepts
  (`AppView`, `History`); event handlers are intent (`open_project`,
  `confirm_kill`); `set_<field>` mutates in place, `with_<field>` consumes and
  returns `Self`; boolean readers are `is_<adjective>` / `has_<noun>` — never
  `can_`; callbacks are `on_<event>`; render helpers are `render_<region>`.
  Types `UpperCamelCase`, everything else `snake_case`, constants
  `SCREAMING_SNAKE_CASE`. Zero-based indices are `ix`, stable identity is `Id`.
- **Precise domain words, not interchangeable ones**: `selected` (membership) ≠
  `focused` (keyboard target) ≠ `hovered` (pointer) ≠ `confirmed` (activation).
  `open/close` = overlay state; `show/hide` = transient; `expand/collapse` =
  structure. `index` = position; `id` = identity. `size` = control tier;
  `width/height/bounds` = geometry.
- **Avoid vague names**: `data`, `handle_action`, `update_ui`, `process`,
  `manager`. Name the domain concept.
- **Doc comments**: `//!` on modules to say _why the module exists and what it
  owns_; `///` on public items to say what it does and who owns the state.
  Document invariants, defaults, and surprising lifecycle constraints — do not
  narrate obvious builder calls. The existing files (`collect/mod.rs`,
  `app.rs`, `lib.rs`) are the house style: dense module headers, no filler.
- **Error handling**: this is an application, not a library. Prefer
  `Option`/`Result` + `?` + `ok()?` and a degrade-to-`None` policy over error
  types. There is no `thiserror`/`anyhow` dependency and no need to add one; the
  only hard failure is "`/proc` is unusable" → exit with a clear stderr message.
  Log-once via `logger::once(key, msg)` for anything that would repeat every
  tick (a missing sensor, a failed mount).
- **`unsafe` is allowed only for libc FFI, and only inside `src/collect/`**
  (`getifaddrs`, `getloadavg`, `sysconf`, `setpriority` live there today). Wrap
  each one in a small safe function, give it a `// SAFETY:` comment stating the
  invariant, and never let `unsafe` reach `ui/` or `model.rs`. Prefer a `nix`
  wrapper when one exists.
- **No broad `allow` attributes.** If a lint is genuinely wrong for a line, add
  a targeted `#[allow(clippy::exact_name)]` with a short comment saying why.
- **Put lint policy in `[lints]` in `Cargo.toml`** (stable since 1.74) rather
  than in a wrapper script. There is no `[lints]` section yet — add one the
  first time a project-wide rule is needed, and keep it short enough to read.
- **Format with `rustfmt`** (`cargo fmt --all`); do not hand-format.

---

## 6. GPUI facts that were already verified — do not re-discover them the hard way

These were confirmed by reading the vendored sources, not by guessing. They are
the ones that cost a day each.

- **There is no `Timer::after`.** `Timer` is only built by
  `Executor::timer(duration)` and is a `Future<Output = ()>`. The tick loop is
  `cx.spawn(async move |this, cx| loop { cx.background_executor().timer(d).await; ... })`.
  `WeakEntity::update` returning `Err` means the view is gone → **break the
  loop** (otherwise a detached loop wakes forever).
- **`KeyBinding::new(keystrokes, action, context: Option<&str>)`** — three
  arguments. Multi-key sequences are space-separated.
- **`FontWeight` is a tuple struct**: `FontWeight(500.0)`, not
  `FontWeight::Medium`.
- **There is no `Sizable::medium()`** — `Size::Medium` is `#[default]`, so just
  omit it. Others: `xsmall()`, `small()`, `large()`, `with_size(..)`.
- **`h_flex()` vs `v_flex()` are asymmetric.** `h_flex()` = row +
  `items_center()`; `v_flex()` = column with **no** cross-axis centering. A
  full-height column inside a row needs an explicit `h_full()`. Cross-axis
  stretching in GPUI is `items_*`, not `justify-*`.
- **`min_h_0()` is mandatory** on a scrolling flex child, or it refuses to
  shrink and pushes siblings out of the container.
- **Charts fill their parent.** `AreaChart`/`LineChart` take no width/height;
  `request_layout` hardcodes `Size::full()`. Put the height on the parent
  (`.h(px(200.))`). Set `.point_count(n)` (the ring capacity) or the x axis
  breathes on every tick. Pin `.y_domain(0.0, 100.0)` for percentages, or a
  flat/idle series draws nothing.
- **`Theme`/`ActiveTheme`**: `cx.theme()` needs `use gpui_kit::component::ActiveTheme;`.
  `Theme` derefs to `ThemeColor`, so `cx.theme().accent` / `.muted` /
  `.border` / `.primary` are plain `Hsla`. Real token names end in
  `_foreground` (`muted_foreground`, not `muted_text`). `Theme::change(mode,
window, cx)` switches globally. **Never hard-code a colour or a radius** —
  it breaks custom themes. Use tokens; prefer
  `ThemeStyled::rounded_full_style(cx)` over `rounded_full()`.
- **`window_border()` insets the whole app by 20px on Linux.** `SHADOW_SIZE` is
  `px(20.0)` on Linux and `px(0.0)` elsewhere
  (`gpui-component-0.7.0/src/window_border.rs`); the border element pads its
  content by it, paints that band transparent (`set_client_inset`) and reports it
  to the compositor as the client inset. A default-options window therefore
  shows a 20px gutter on every side and the app reads as content sitting inside
  a container, with the desktop visible through it. `.shadow_size(px(0.))` makes
  the window bounds the app bounds. The resize bands are **centred on the frame
  edge**, so with no inset half of each band falls outside the window — widen
  them (`.resize_hit_size(..)`) when the shadow is zero.
- **`flex_1()` is `flex: 1 1 0%` — `flex_basis: relative(0.)`, not `auto`.**
  (`gpui-pre-0.3.7/src/styled.rs:181`.) Combined with the `min_h_0()` that
  disables the content-based automatic minimum, a column of `flex_1` panels has
  an **intrinsic height of zero**: a `flex_none` row that is sized by its content
  collapses to nothing. Every content-sized row must keep at least one
  `flex_none` child (`ui::chrome::panel_auto`) to have any height at all.
  `flex_auto()` is the `1 1 auto` variant; the chunk suffix ramp has no `flex_2`,
  so weighting is `.flex_grow(n)`.
- **Dialogs are application state here, not an imperative API.**
  `AppView::dialog` is an enum and a dialog renders when its variant is active;
  closing sets it back to `None`. gpui-kit _does_ offer an imperative
  `WindowExt` dialog entity — this repo deliberately does not use it, because
  then nothing can be left open by a forgotten callback. Keep it that way.
- **No `.mono()` helper.** Monospace is
  `.font_family(cx.theme().mono_font_family.clone())`.
- **`Progress` is not a `ParentElement`** (no label inside it);
  `ProgressCircle` is.
- **Check `src/lib.rs`'s `pub use` list before assuming a flat path.** Some
  types are flat (`TitleBar`, `VirtualList`, `h_flex`, `IconName`, `Theme`),
  others are module-qualified (`component::status_bar::StatusBar`,
  `component::progress::*`, `component::chart::*`).
- **`cx.notify()` belongs in the tick, never in `render()`** — calling it
  during render schedules another frame and spins the UI thread forever.
- **`ElementId`s must be derived from stable domain identity** (pid, mount
  name), never from a list index, or keyed state gets mixed up on reorder.
- **`component::IconName` is a compatibility SUBSET.** The full Lucide set is
  `gpui_kit::assets::IconName` — `Activity`, `SunMoon`, `ArrowUpDown`, `Skull`
  and most others only exist there. `Icon::new` accepts anything implementing
  `IconNamed`, so `Icon::new(gpui_kit::assets::IconName::Activity)` works.
- **`window_border()` lives at `gpui_kit::component::window_border`**, not at the
  crate root.
- **A `TitleBar` must be _rendered_, not just configured.** `TitleBar::window_options()`
  reserves the 34px strip and lets the compositor drag by it, but nothing is
  painted there unless `TitleBar::new().child(..)` is in the element tree. It
  implements `ParentElement`.
- **`WindowOptions.app_id`** is what Wayland/GNOME matches against the desktop
  file's `StartupWMClass` to resolve the window's name and icon. Setting only the
  window title leaves the taskbar showing "Unknown".
- **Charts are interactive by default** — `.interactive(false)` removes the
  hitbox and with it the crosshair and tooltip. `tooltip_value` is
  `Fn(&T, f64)` on `LineChart` but `Fn(&T, usize, f64)` on Area/Bar/Radar/
  Candlestick (the extra arg is the series index). `y_tick_format` is
  `Fn(f64) -> impl Into<SharedString>`. **`y_padding` defaults to 10px of
  headroom**, which is why a pinned `y_domain(0, 100)` labels its top tick 112.2.
- **Return a concrete `Div`, not `impl IntoElement`, from anything a caller's
  `&mut Context` is passed to.** An opaque return type keeps the caller's borrow
  alive, and the next `cx` use fails with E0502. `chrome.rs` documents this.
- **Read `cx.theme()` into `Copy` locals before any mutable `cx` use** in the
  same function, for the same reason.
- **`.children(iter.map(|x| f(x, cx)))` does not compile** when `cx` is
  `&mut Context` — "captured variable cannot escape `FnMut` closure body". Use a
  `for` loop so each row reborrows.
- **`ThemeMode` has no `System` variant**, `Theme::change` ignores its `window`
  argument (it is global), and **gpui-pre has no Linux colour-scheme detection** —
  `window.appearance()` never consults the desktop. Read
  `gsettings get org.gnome.desktop.interface color-scheme` instead.
- **`ClickEvent` is an enum** (`Mouse`/`Keyboard`), so `click_count` is not on it.
  Use `on_mouse_down(button, ..)` and read `MouseDownEvent::click_count`, or
  match the enum. `hover`'s closure takes `StyleRefinement` **by value**, so
  `.hover(|s| s.bg(color))` works.
- **`Progress` colours itself from the theme's `progress_bar` token**, which is
  near-black in the light theme; pass `.color(..)` explicitly. `cx.reduce_motion()`
  exists if motion ever needs respecting.
- **`Theme::accent` is a _surface_, not ink.** The crate's own doc comment says
  "Used for accents such as hover background on MenuItem, ListItem"; its matching
  text colour is `accent_foreground`. Used as a text colour, `accent` is
  `neutral-800` (#262626) on the dark theme and `neutral-100` (near-white) on the
  light one — so it is invisible in one mode whichever theme is set. This shipped
  once as a selected sort pill with near-black text on a near-black panel.
  `primary`, `secondary`, `danger` and `info` have the same shape: each is a
  background with a `_foreground` partner for the text that sits on it.
- **The theme's `chart.1..5` are the same values in both modes** — a pale blue
  (#93c5fd) through to a navy (#1e40af). A graph drawn straight from them is
  unreadable in one theme whichever end it is handed: the pale end vanishes on
  white, the navy end on near-black. Use `ui::theme::stroke(cx, i)`, which
  reverses the ramp in light mode so every stroke sits on the legible half, and
  references no literal colour while doing it.

---

## 7. Testing

Tests are **in-crate `#[cfg(test)] mod tests`** next to the code they test,
reading fixtures from `tests/fixtures/` (via `CARGO_MANIFEST_DIR`). There is no
`tests/parsers.rs` integration file — if any doc tells you to run
`cargo test --test parsers`, that doc is stale.

```bash
cargo test --lib            # the whole suite, no GPUI binary built
cargo test --lib parse_pid_stat
```

Rules:

- **Never read the live machine and assert a value.** It passes here and fails
  everywhere else. Fixtures only. The single exception is a smoke test that the
  collector returns `core_count > 0`.
- **Cover the delta maths with two synthetic samples**, including
  "first sample is always 0 %" and counter rollover.
- **Test the traps explicitly**: a `comm` containing a space and a parenthesis
  (`proc_pid_stat_weird_comm.txt`), a missing `MemAvailable`, `sectors * 512`,
  `\040` mount escapes, ring capacity, an invalid regex filter matching nothing
  without panicking.
- **Guard against vacuous green.** A test command that discovers zero tests
  exits 0. Keep the suite ≥ 100 tests and say the count in your report; treat
  "0 tests run" as a failure, not a pass.
- **A fixture-based test must not silently pass when the fixture is missing.**
  `collect/proc.rs`'s fixture test currently reads the file, and on a read error
  prints a line and `return`s — a green result with zero assertions. New
  fixture tests should `panic!`/`assert!` on a missing fixture instead, or the
  suite will pass on a machine (or CI checkout) that never had the file.
- **UI tests are optional.** `#[gpui_kit::test]` / `VisualTestContext` exist but
  it is unverified whether they render without a Vulkan device. The data layer
  carries the real risk — test that first, and never make CI depend on a GPU.
- Add a regression test before fixing a bug whenever the failure is
  deterministically reproducible.

---

## 8. Rendering and performance rules

The app is on a 2 s tick. These are the rules that keep it at 60 fps.

1. **`pull()` allocates; `render()` does not.** Take the snapshot, fold history,
   sort/filter/tree the process list, build chart `Vec`s — all in `pull()`,
   once per tick. Sorting 2 000 rows inside `render()` is the classic 60×-per-
   second mistake.
2. **Never clone the `Snapshot`.** It owns a `Vec` of every process (~100 KB);
   share it as `Arc<Snapshot>` and borrow it in `render()`.
3. **Never build a `Vec` per row or per frame.**
4. **Virtualize long lists** and give the container a fixed/max height with
   `overflow_y_hidden()`.
5. **Notify the narrowest entity after a coherent state change**, not per field.
6. **Measure before caching** (targets: one `tick` < 20 ms with 500 PIDs and
   8 mounts; one `render()` < 4 ms; sort+filter+tree of 2 000 procs < 5 ms;
   flat RSS after a 30-minute soak).
7. **The `Ring` only pushes/pops from the front.** Never splice — a gap shifts
   every later point left.

---

## 9. Traps, and things that look like bugs but are not

**Traps** (each has already cost someone a day):

- Splitting `/proc/<pid>/stat` on whitespace — `comm` is in parens and can
  contain spaces _and_ parens. Split on the **last** `)`, then index `rest[N-3]`.
- `/proc/meminfo` is kB; disk sectors are always 512 B; sysfs temps are milli-°C;
  `scaling_cur_freq` is kHz.
- In `/etc/passwd` the UID is the **third** field — the second is the `x`
  placeholder. Parsing field 2 silently yields an empty uid map, and then every
  process renders its user as a bare number.
- `statvfs()` on a stale NFS/CIFS/sshfs mount **blocks for minutes**. That is
  why collection is on a thread. Log once, add to a permanent `ignore_list`,
  never retry.
- u64 network/disk counters wrap: use `wrapping_sub` and reject absurd deltas
  (a "1.8e19 B/s" spike is a wrap, not traffic).
- `/proc/<pid>/io` and `/proc/<pid>/smaps` are expensive/permission-gated —
  detail view only, never for the whole list (`smaps` costs ~20× CPU, btop's
  own note).
- Including an `nvme` hwmon sensor as a CPU temperature; a `→` CPU wattage
  needs `cap_perfmon` — latch `supports_watts = false` and hide the row.

**Not bugs** (do not "fix" these):

- CPU reads 0 % on the **first** tick — a delta needs two samples. Render `—`
  until `history.len() >= 2`.
- Temps are missing inside containers/VMs (no `/sys/class/hwmon`).
- A process shows > 100 % CPU when `proc_per_core = true` — that is relative to
  all cores, exactly like `top`/`btop`.
- A core reads 0.0 MHz — parked governor, or the file is unreadable in a VM.
- The network graph jumps scale — that is the auto-scaler.
- `ps` disagrees about process CPU % — `ps` averages since start, we take the
  instantaneous delta. Compare against `btop`, never `ps`.
- The binary is ~100 MB. That is the GPUI stack; `lto`/`strip` are already on.

---

## 10. Definition of done

Before you say a task is finished:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --lib                      # >= 100 tests, state the count
cargo check --all-targets
grep -rn "unwrap()" src/ | grep -v "#\[cfg(test)\]"     # only the startup exceptions
grep -rn "gpui" src/collect/ src/model.rs              # empty
```

Then, if the change is user-visible, **run the app** (`cargo run`) and check it
against the manual matrix: launches on both X11 and Wayland; numbers match
`btop` beside it; a stale mount does not freeze it; `kill` on a root-owned
process surfaces EPERM without dying; resizing small and large does not break
the layout; switching theme reveals no hard-coded colours. "It compiles and
clippy is clean" is not evidence that the UI works.

---

## 11. Where to read more

| Need                                              | Read                                                             |
| ------------------------------------------------- | ---------------------------------------------------------------- |
| Why these dependencies and not others             | `docs/01-research.md`                                            |
| Threading model, module boundaries, unit table    | `docs/02-architecture.md`                                        |
| Exact `/proc` + `/sys` paths and maths per metric | `docs/03-data-layer.md`                                          |
| GPUI boot, panels, charts, key maps               | `docs/04-ui.md`                                                  |
| Config file format and theme mapping              | `docs/05-config-and-theming.md`                                  |
| Build deps, caps, packaging, manual matrix        | `docs/06-build-and-ship.md`                                      |
| Ordered task list                                 | `docs/07-tasks.md`                                               |
| Trap list with mitigations                        | `docs/08-risks-and-edge-cases.md`                                |
| gpui-kit architecture + naming rules              | <https://gpui-kit.com/docs/coding-guides.md>                     |
| gpui-kit component APIs                           | `https://gpui-kit.com/component/<name>.md`, index `.../llms.txt` |
| Exact GPUI signatures                             | the vendored sources under `~/.cargo/registry/src/*/gpui-*/src/` |
| Upstream btop behaviour                           | `btop-src/` (C++, local only)                                    |

**Two caveats about `docs/`:**

1. It is **gitignored** — local working material, not part of the repo. On a
   fresh clone it does not exist. Everything load-bearing is in this file; the
   docs are depth, not prerequisites.
2. Two things in it are already stale: `docs/06` §5.1 describes an integration
   test file (`tests/parsers.rs`) that was never created — the tests are
   in-crate — and `docs/07-tasks.md`'s progress list was pre-filled
   optimistically. **Trust `git log`, the source, and this file over the task
   checklist.**

## 12. Keeping this file honest

`AGENTS.md` is always-on context, so it must stay small and true. When you learn
something durable — a corrected API fact, a new trap, a changed command —
update this file in the same commit as the change that taught you. Delete a rule
once it stops being relevant; a stale instruction is worse than no instruction.
Do not paste big excerpts of the plan docs in here — link to them.
