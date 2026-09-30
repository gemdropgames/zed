# WASM Emulator Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** zedgg runs the GGO emulator as a wasmtime-hosted `ggo_emu.wasm` loaded from a pluggable source (bundled, local file, HTTP URL, Forgejo release), swappable at runtime without restarting zed.

**Architecture:** The ggo repo's existing browser C-ABI front end (`tools/ggo-emu/src/wasm.rs`) grows the exports zed's drive loop needs; shared types/constants move to a tiny `ggo-emu-abi` crate and the save-file format to `ggo-savefile`, both of which zed links natively. A new zed crate `ggo_emu_wasm` hosts the module (`LoadedEmulator` / `WasmEmu` / `WasmApu`), implements the `EmulatorSource` trait for four sources, and owns an `EmuRuntime` gpui global driven by `~/.ggo/emulator.json`. `drive.rs`, `link.rs` and the audio preview are ported onto it; running sessions restart when the module changes.

**Tech Stack:** Rust, wasmtime 36 (sync, epoch interruption), gpui, zed `http_client` / `fs` / `paths`, serde_json, sha2.

**Spec:** `docs/superpowers/specs/2026-09-30-wasm-emulator-design.md`

## Global Constraints

- Two repos: zed at `/home/clay/projects/zed` (branch `wasm-emulator`, already created), ggo at `/home/clay/projects/ggo` (branch `wasm-emulator`, created in Task 2). zed's `Cargo.toml` path-deps `../ggo`, so zed builds against whatever the ggo checkout has.
- Prefix EVERY cargo / clippy / install / build command with `nice -n 19 ionice -c2 -n7` (the user games during builds).
- Verification gates the commit: chain on real exit codes, e.g. `nice -n 19 ionice -c2 -n7 ./script/clippy -p ggo_emu_wasm && nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_wasm && git commit ...`. Never `echo` after a check.
- Commit messages: short, no Claude/AI co-author trailer, no "Generated with". zed commits prefixed `ggo:` (repo convention, e.g. `ggo: one shared picker card behind every panel's fuzzy modal`). Follow ggo's own `CLAUDE.md`/`tools/AGENTS.md` in the ggo repo.
- zed rules (`CLAUDE.md`): no `unwrap()` in non-test code, no `let _ =` on fallible calls (use `.log_err()` / `?` / explicit match), no `mod.rs`, crate lib root via `[lib] path = "src/<name>.rs"`, full-word variable names, comments only for non-obvious "why".
- ABI version: `ggo_emu_abi::ABI_VERSION = (1 << 16) | 0`. zed accepts major == 1 and minor >= 0.
- Config file: `~/.ggo/emulator.json`; absent file means `{"source":"bundled"}`.
- Wasm artifact: `cargo build -p ggo-emu --lib --release --target wasm32-unknown-unknown` run in `/home/clay/projects/ggo/tools` → `/home/clay/projects/ggo/tools/target/wasm32-unknown-unknown/release/ggo_emu.wasm`.
- Bench cart for Task 1: `/home/clay/projects/ggo/gateware/build/emd-project/game.ggo`.
- Deviations from spec (deliberate, ponytail): no `.cwasm` precompile cache (compile runs off-thread; add if load time bites); load errors surface as a status line on the emu panel + `log::error!` rather than a workspace toast (the runtime has no window).

## File Structure

ggo repo:
- Create `tools/ggo-emu-abi/{Cargo.toml,src/lib.rs}` — ABI version, status codes, screen/PPU/APU constants, `rgb565_to_argb`, `PpuSnapshot`/`OamEntry`/`MapCell` + byte codec.
- Create `tools/ggo-savefile/{Cargo.toml,src/lib.rs}` — moved from `ggo-emu-core/src/savefile.rs`.
- Modify `tools/Cargo.toml` (workspace members), `tools/ggo-emu-core/{Cargo.toml,src/lib.rs,src/ppu.rs,src/peripherals.rs,src/apu.rs,src/assets.rs}`.
- Modify `tools/ggo-emu/src/wasm.rs` (new exports + host import), `tools/ggo-emu/web/ggo-emu.js` (import stub).

zed repo:
- Create `crates/ggo/emu_wasm/Cargo.toml`, `src/ggo_emu_wasm.rs` (engine, `LoadedEmulator`, `WasmEmu`, `WasmApu`), `src/sources.rs` (trait + 4 impls), `src/runtime.rs` (`EmuRuntime`, config, cache), `src/fixture.rs` (moved from `emu_panel/src/drive.rs`), `bundled/ggo_emu.wasm`.
- Create `script/update-bundled-ggo-emu`.
- Modify `Cargo.toml` (workspace members + deps), `crates/ggo/emu_panel/{Cargo.toml,src/drive.rs,src/link.rs,src/debug.rs,src/audio.rs,src/hardware.rs,src/ggo_emu_panel.rs,src/viewer_run.rs}`, `crates/ggo/audio_panel/{Cargo.toml,src/preview.rs}`, `crates/ggo/emu_mcp/src/tools.rs`, `crates/zed/src/main.rs` (or wherever GGO crates `init`).

---

### Task 1: Perf gate spike (throwaway)

Answers one question: can wasmtime run the emulator fast enough? Nothing here is kept.

**Files:** scratch only — `$SCRATCH/emu-bench/` where `$SCRATCH=/tmp/claude-1000/-home-clay-projects-zed/ef67b0a0-68d9-4392-bd2a-e1136226c4cf/scratchpad`.

- [ ] **Step 1: Build the current wasm**

```bash
cd /home/clay/projects/ggo/tools && nice -n 19 ionice -c2 -n7 cargo build -p ggo-emu --lib --release --target wasm32-unknown-unknown
ls -la target/wasm32-unknown-unknown/release/ggo_emu.wasm
```

- [ ] **Step 2: Create the bench project**

`$SCRATCH/emu-bench/Cargo.toml`:
```toml
[package]
name = "emu-bench"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow = "1"
wasmtime = { version = "36", default-features = false, features = ["runtime", "cranelift"] }
ggo-emu-core = { path = "/home/clay/projects/ggo/tools/ggo-emu-core" }

[profile.release]
debug = false
```

`$SCRATCH/emu-bench/src/main.rs`:
```rust
use std::time::{Duration, Instant};
use anyhow::{Context, Result};
use ggo_emu_core::{cart::Cart, cpu::Cpu, mmu::Mmu, peripherals::Peripherals, run::{run_until_event, FrameEvent}, sandbox};

const CART: &str = "/home/clay/projects/ggo/gateware/build/emd-project/game.ggo";
const WASM: &str = "/home/clay/projects/ggo/tools/target/wasm32-unknown-unknown/release/ggo_emu.wasm";
const CALLS: usize = 600;

fn report(name: &str, mut times: Vec<Duration>, vsyncs: usize) {
    times.sort();
    let total: Duration = times.iter().sum();
    let mean = total / times.len() as u32;
    let p99 = times[times.len() * 99 / 100];
    println!("{name}: calls={} vsyncs={vsyncs} mean={mean:?} p99={p99:?} total={total:?}", times.len());
}

fn native(bytes: &[u8]) -> Result<()> {
    let cart = Cart::parse(bytes).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let (vram, ram) = ggo_emu_core::assets::pool_demand(cart.toc.as_deref(), None);
    let plan = sandbox::plan(sandbox::ARENA_MAX_LEN, vram, ram, cart.header.ram_needed.max(sandbox::MIN_ARENA));
    let mut mmu = Mmu::with_plan(plan);
    mmu.load_cart_body(&cart.body);
    let mut cpu = Cpu::new(sandbox::XIP_BASE.wrapping_add(cart.header.entry_offset));
    ggo_emu_core::cpu::enter_sandbox(&mut cpu, &plan);
    let mut p = Peripherals::for_cart(1, &cart.header);
    p.perf.enable();
    if let Some(toc) = cart.toc { p.assets.set_toc(toc); }
    let (mut times, mut vsyncs, mut ticks) = (Vec::new(), 0, 0u32);
    for _ in 0..CALLS {
        p.set_ticks_ms(ticks);
        let start = Instant::now();
        let (event, _) = run_until_event(&mut cpu, &mut mmu, &mut p, 5_000_000, false);
        times.push(start.elapsed());
        match event { FrameEvent::Vsync(_) => { vsyncs += 1; ticks += 16 } FrameEvent::Budget => {} other => { println!("native ended: {other:?}"); break } }
    }
    report("native", times, vsyncs);
    Ok(())
}

fn wasm(bytes: &[u8]) -> Result<()> {
    let engine = wasmtime::Engine::default();
    let compile = Instant::now();
    let module = wasmtime::Module::from_file(&engine, WASM)?;
    println!("compile: {:?}", compile.elapsed());
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = wasmtime::Linker::new(&engine).instantiate(&mut store, &module)?;
    let memory = instance.get_memory(&mut store, "memory").context("memory")?;
    let alloc = instance.get_typed_func::<u32, u32>(&mut store, "ggo_alloc")?;
    let new = instance.get_typed_func::<(u32, u32, u32, u32), u32>(&mut store, "ggo_emu_new")?;
    let run = instance.get_typed_func::<(u32, u32, u32), u32>(&mut store, "ggo_emu_run_frame")?;
    let ptr = alloc.call(&mut store, bytes.len() as u32)?;
    memory.write(&mut store, ptr as usize, bytes)?;
    let emu = new.call(&mut store, (ptr, bytes.len() as u32, 1, 0))?;
    anyhow::ensure!(emu != 0, "ggo_emu_new returned null");
    let (mut times, mut ticks) = (Vec::new(), 0u32);
    for _ in 0..CALLS {
        let start = Instant::now();
        let status = run.call(&mut store, (emu, 0, ticks))?;
        times.push(start.elapsed());
        ticks += 16;
        if status != 0 { println!("wasm ended: status {status}"); break }
    }
    report("wasm", times, 0);
    Ok(())
}

fn main() -> Result<()> {
    let bytes = std::fs::read(CART)?;
    native(&bytes)?;
    wasm(&bytes)
}
```

- [ ] **Step 3: Run it**

```bash
cd $SCRATCH/emu-bench && nice -n 19 ionice -c2 -n7 cargo run --release
```
Expected: two report lines. If the API names above don't match `ggo-emu-core` exactly (e.g. `load_cart_body` returns a bool you must bind), adjust the bench — it is throwaway.

- [ ] **Step 4: Report, do not commit**

Report `compile`, native mean/p99, wasm mean/p99, ratio. **Gate:** wasm mean ≤ 8 ms and p99 ≤ 14 ms per call (≥2× headroom inside the 16.67 ms frame at 1×). If the gate fails, STOP the whole plan and report to the controller; do not start Task 2.

---

### Task 2: `ggo-emu-abi` crate (ggo repo)

**Files:**
- Create: `/home/clay/projects/ggo/tools/ggo-emu-abi/Cargo.toml`, `/home/clay/projects/ggo/tools/ggo-emu-abi/src/lib.rs`
- Modify: `/home/clay/projects/ggo/tools/Cargo.toml` (members), `tools/ggo-emu-core/Cargo.toml`, `tools/ggo-emu-core/src/ppu.rs`, `src/peripherals.rs`, `src/apu.rs`

**Interfaces:**
- Produces (crate `ggo_emu_abi`):
  - `pub const ABI_VERSION: u32 = 1 << 16;` `pub const fn abi_major(v: u32) -> u16`, `pub const fn abi_minor(v: u32) -> u16`
  - Turn status codes: `STATUS_VSYNC = 0, STATUS_EXITED = 1, STATUS_FAULTED = 2, STATUS_OOM = 3, STATUS_BUDGET = 4` (u32)
  - Flags for `ggo_emu_new_ex`: `NEW_FLAG_LOG_SINK: u32 = 1`, `NEW_FLAG_HOST_ASSETS: u32 = 2`
  - `SCREEN_WIDTH, SCREEN_HEIGHT, SCREEN_PIXELS` (usize), `rgb565_to_argb(u16) -> u32`
  - PPU: `TILE_PX, TILE_BYTES, VRAM_TILE_CAP, MAP_W, MAP_H, LAYER_COUNT, PALETTES, PAL_ENTRIES, OAM_ENTRIES, BANK_BGFG, BANK_SPRITE`, `OamEntry`, `MapCell`, `PpuSnapshot` (all existing methods), `PpuSnapshot::encode(&self, out: &mut Vec<u8>)`, `PpuSnapshot::decode(bytes: &[u8]) -> Option<PpuSnapshot>`, `PpuSnapshot::ENCODED_LEN: usize`
  - APU: `MIX_RATE`, `RING_LEN`, `ONE_SHOT` plus whatever constants `MIX_RATE` is computed from.
- `ggo-emu-core` keeps every existing public path working via `pub use ggo_emu_abi::...` in `ppu`, `peripherals`, `apu`.

- [ ] **Step 1: Branch the ggo repo**

```bash
cd /home/clay/projects/ggo && git status --short
```
If output is non-empty, STOP and report (someone is working in this checkout). Else `git switch -c wasm-emulator`.

- [ ] **Step 2: Scaffold the crate**

`tools/ggo-emu-abi/Cargo.toml`:
```toml
[package]
name = "ggo-emu-abi"
version = "0.1.0"
edition = "2021"
description = "The ggo_emu.wasm host ABI: version, status codes, shared constants and the PPU snapshot wire format. Any change here bumps ABI_VERSION's major."
license = "MIT OR Apache-2.0"

[lib]
name = "ggo_emu_abi"
path = "src/lib.rs"
```
Add `"ggo-emu-abi"` to `members` in `tools/Cargo.toml`. Add `ggo-emu-abi = { path = "../ggo-emu-abi" }` to `tools/ggo-emu-core/Cargo.toml` `[dependencies]`.

- [ ] **Step 3: Move definitions**

In `src/lib.rs` start with `#![no_std]` and `extern crate alloc;` and the ABI/status/flag constants listed above. Then MOVE (cut from ggo-emu-core, paste here) — do not duplicate:
- `SCREEN_WIDTH/HEIGHT/PIXELS` and `rgb565_to_argb` from `ggo-emu-core/src/peripherals.rs` (lines ~21-23, ~266-274; move its unit test too).
- From `ggo-emu-core/src/ppu.rs`: the constants listed above, `OamEntry`, `MapCell` (+ impls), `PpuSnapshot` (+ `Default` + `impl PpuSnapshot`, i.e. everything from line ~1627 to the end of `impl PpuSnapshot` except `Ppu::snapshot_into`/`Ppu::snapshot`, which stay on `Ppu`). If any moved item depends on something else in `ppu.rs`, move that too if it is pure data; if it is not pure data, stop and report.
- `MIX_RATE`, `RING_LEN`, `ONE_SHOT` and their input constants from `apu.rs`.
Use `alloc::vec::Vec`/`alloc::vec!` in the moved code.

In each source file in ggo-emu-core, replace the removed items with re-exports, e.g. in `ppu.rs`:
```rust
pub use ggo_emu_abi::{
    BANK_BGFG, BANK_SPRITE, LAYER_COUNT, MAP_H, MAP_W, MapCell, OAM_ENTRIES, OamEntry, PAL_ENTRIES,
    PALETTES, PpuSnapshot, TILE_BYTES, TILE_PX, VRAM_TILE_CAP,
};
```
(and the analogous lines in `peripherals.rs`, `apu.rs`). Existing callers must compile unchanged.

- [ ] **Step 4: Write the failing codec test** (in `ggo-emu-abi/src/lib.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn sample() -> PpuSnapshot {
        let mut snapshot = PpuSnapshot::default();
        snapshot.tiles[3] = 0xAB;
        snapshot.maps[2][17] = 0x1234;
        snapshot.palettes[1][5] = 0xF800;
        snapshot.oam[7] = [1, 2, 3, 4, 5, 6, 7, 8];
        snapshot.scroll[1] = (300, 7);
        snapshot.layer_enable[3] = true;
        snapshot.layer_prio[0] = 2;
        snapshot
    }

    #[test]
    fn snapshot_round_trips() {
        let mut bytes = Vec::new();
        sample().encode(&mut bytes);
        assert_eq!(bytes.len(), PpuSnapshot::ENCODED_LEN);
        assert_eq!(PpuSnapshot::decode(&bytes), Some(sample()));
    }

    #[test]
    fn snapshot_decode_rejects_wrong_length() {
        let mut bytes = Vec::new();
        sample().encode(&mut bytes);
        bytes.pop();
        assert_eq!(PpuSnapshot::decode(&bytes), None);
    }
}
```
`PpuSnapshot` needs `#[derive(Debug, Clone, PartialEq)]` for these asserts; add it if absent.

- [ ] **Step 5: Run to verify it fails**

`cd /home/clay/projects/ggo/tools && nice -n 19 ionice -c2 -n7 cargo test -p ggo-emu-abi` → FAIL (`encode`/`ENCODED_LEN` not found).

- [ ] **Step 6: Implement the codec**

Layout, all little-endian, in this order: `tiles` bytes; each of `LAYER_COUNT` maps as `MAP_W*MAP_H` u16; both palettes as `PALETTES*PAL_ENTRIES` u16; `OAM_ENTRIES` × 8 bytes; `LAYER_COUNT` × (u16 x, u16 y); `LAYER_COUNT` × u8 enable (0/1); `LAYER_COUNT` × u8 prio.

```rust
impl PpuSnapshot {
    pub const ENCODED_LEN: usize = VRAM_TILE_CAP * TILE_BYTES
        + LAYER_COUNT * MAP_W * MAP_H * 2
        + 2 * PALETTES * PAL_ENTRIES * 2
        + OAM_ENTRIES * 8
        + LAYER_COUNT * 4
        + LAYER_COUNT * 2;

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.reserve(Self::ENCODED_LEN);
        out.extend_from_slice(&self.tiles);
        for map in &self.maps {
            map.iter().for_each(|cell| out.extend_from_slice(&cell.to_le_bytes()));
        }
        for palette in &self.palettes {
            palette.iter().for_each(|color| out.extend_from_slice(&color.to_le_bytes()));
        }
        self.oam.iter().for_each(|entry| out.extend_from_slice(entry));
        for (x, y) in self.scroll {
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
        out.extend(self.layer_enable.iter().map(|&enabled| u8::from(enabled)));
        out.extend_from_slice(&self.layer_prio);
    }

    pub fn decode(bytes: &[u8]) -> Option<PpuSnapshot> {
        if bytes.len() != Self::ENCODED_LEN {
            return None;
        }
        let mut rest = bytes;
        let mut take = |n: usize| -> &[u8] {
            let (head, tail) = rest.split_at(n);
            rest = tail;
            head
        };
        let u16s = |raw: &[u8]| -> Vec<u16> {
            raw.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect()
        };
        let mut snapshot = PpuSnapshot::default();
        snapshot.tiles.copy_from_slice(take(VRAM_TILE_CAP * TILE_BYTES));
        for map in &mut snapshot.maps {
            *map = u16s(take(MAP_W * MAP_H * 2));
        }
        for palette in &mut snapshot.palettes {
            *palette = u16s(take(PALETTES * PAL_ENTRIES * 2));
        }
        for entry in &mut snapshot.oam {
            entry.copy_from_slice(take(8));
        }
        for scroll in &mut snapshot.scroll {
            let raw = take(4);
            *scroll = (u16::from_le_bytes([raw[0], raw[1]]), u16::from_le_bytes([raw[2], raw[3]]));
        }
        for enabled in &mut snapshot.layer_enable {
            *enabled = take(1)[0] != 0;
        }
        snapshot.layer_prio.copy_from_slice(take(LAYER_COUNT));
        Some(snapshot)
    }
}
```
(The length check up front is what makes the `split_at`/indexing safe. If the borrow checker rejects the `take` closure borrowing `rest` mutably while `u16s` is in scope, turn `take` into a small `struct Reader<'a>(&'a [u8])` with a `fn take(&mut self, n) -> &'a [u8]`.)

- [ ] **Step 7: Verify and commit**

```bash
cd /home/clay/projects/ggo/tools && nice -n 19 ionice -c2 -n7 cargo test -p ggo-emu-abi \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo-emu-core \
  && nice -n 19 ionice -c2 -n7 cargo build -p ggo-emu --lib --release --target wasm32-unknown-unknown \
  && git add -A tools/ggo-emu-abi tools/Cargo.toml tools/Cargo.lock tools/ggo-emu-core \
  && git commit -m "ggo-emu-abi: shared host ABI constants and PPU snapshot codec"
```

---

### Task 3: `ggo-savefile` crate (ggo repo)

**Files:**
- Create: `tools/ggo-savefile/Cargo.toml`, `tools/ggo-savefile/src/lib.rs`
- Modify: `tools/Cargo.toml`, `tools/ggo-emu-core/Cargo.toml`, `tools/ggo-emu-core/src/lib.rs`; delete `tools/ggo-emu-core/src/savefile.rs`

**Interfaces:**
- Produces crate `ggo_savefile` with the exact public API of today's `ggo_emu_core::savefile` (`FLUSH_INTERVAL_FRAMES`, `SAVE_HDR_BYTES`, `SAVE_MAGIC`, `SAVE_FORMAT_VERSION`, `SAVE_NAME_PROBES`, `title_hash`, `header`, `header_matches`, `save_name`, `save_path`, `resolve_save_path(card_dir: &Path, title: &str, save_bytes: usize) -> Option<PathBuf>`, `load_save(path: &Path, title: &str, save: &mut [u8])`, `flush_save(path: &Path, title: &str, save: &[u8]) -> std::io::Result<()>`).
- `ggo_emu_core::savefile` keeps resolving (`pub use ggo_savefile as savefile;`, under the same cfg the module has today).

- [ ] **Step 1:** `git mv tools/ggo-emu-core/src/savefile.rs tools/ggo-savefile/src/lib.rs`. Create `Cargo.toml` (`name = "ggo-savefile"`, `[lib] name = "ggo_savefile" path = "src/lib.rs"`, same license/edition as ggo-emu-core; copy any deps savefile.rs uses). Add to workspace members; add as dep of ggo-emu-core. Replace `pub mod savefile;` in `ggo-emu-core/src/lib.rs` with `pub use ggo_savefile as savefile;` (keep any existing `#[cfg]` on it). Fix `crate::` paths inside the moved file.
- [ ] **Step 2: Verify and commit** — its existing unit tests move with it and are the test:

```bash
cd /home/clay/projects/ggo/tools && nice -n 19 ionice -c2 -n7 cargo test -p ggo-savefile \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo-emu-core -p ggo-emu \
  && git add -A tools && git commit -m "ggo-savefile: split the save-file format out of ggo-emu-core"
```

---

### Task 4: Pluggable card-dir reader (ggo repo)

**Files:**
- Modify: `tools/ggo-emu-core/src/assets.rs` (~lines 241-330 and tests ~492-650)

**Interfaces:**
- Produces: `pub type CardReader = Box<dyn FnMut(&str) -> Option<Vec<u8>>>;` and `AssetStore::set_card_reader(&mut self, reader: CardReader)`. `set_card_dir(dir: PathBuf)` keeps working (installs a `std::fs` reader over `dir`). The sandbox check (no absolute, no `..`) stays in `load` and runs BEFORE any reader is called.

- [ ] **Step 1: Failing test** (add to the `tests` module in `assets.rs`; mirror the construction style of the existing `toc_miss_falls_back_to_card_dir` test for the store, `dest`, pool arguments):

```rust
#[test]
fn toc_miss_falls_back_to_the_card_reader() {
    let mut store = AssetStore::default(); // use whatever constructor the neighbouring tests use
    let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    store.set_card_reader(Box::new({
        let asked = asked.clone();
        move |path| {
            asked.borrow_mut().push(path.to_string());
            (path == "only-in-reader.bin").then(|| vec![0x5Au8; 16])
        }
    }));
    // same pool/dest arguments as toc_miss_falls_back_to_card_dir:
    let handle = /* store.load(b"only-in-reader.bin", dest, pool_base, &mut pool) */;
    assert_ne!(handle, 0);
    assert_eq!(store.load(b"../escape.bin", /* same args */), 0);
    assert_eq!(asked.borrow().as_slice(), ["only-in-reader.bin"]);
}
```
(Fill the `/* */` pieces from the neighbouring test; the last assert proves the `..` path never reached the reader.)

- [ ] **Step 2:** `cargo test -p ggo-emu-core toc_miss_falls_back_to_the_card_reader` → FAIL (no `set_card_reader`).
- [ ] **Step 3: Implement.** Replace the `card_dir: Option<PathBuf>` field with `card_reader: Option<CardReader>`. `set_card_dir` becomes:

```rust
pub fn set_card_dir(&mut self, dir: PathBuf) {
    self.card_reader = Some(Box::new(move |path| std::fs::read(dir.join(path)).ok()));
}

pub fn set_card_reader(&mut self, reader: CardReader) {
    self.card_reader = Some(reader);
}
```
In `load`, step 2 becomes: `let Some(reader) = self.card_reader.as_mut() else { return 0 };` → sandbox check on `Path::new(rel)` (unchanged) → `let Some(bytes) = reader(rel) else { return 0 };` → `self.place(rel, &bytes, ...)`. If other code read `card_dir` (grep `card_dir` in the crate), keep a separate `card_dir` field for it rather than changing that code. If `AssetStore` derives `Debug`/`Clone`, implement `Debug` manually (print `card_reader.is_some()`) and report if `Clone` is required anywhere.
- [ ] **Step 4: Verify and commit**

```bash
cd /home/clay/projects/ggo/tools && nice -n 19 ionice -c2 -n7 cargo test -p ggo-emu-core \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo-emu \
  && git add -A tools/ggo-emu-core && git commit -m "ggo-emu-core: card-dir asset reads go through a pluggable reader"
```

---

### Task 5: wasm ABI additions (ggo repo)

**Files:**
- Modify: `tools/ggo-emu/src/wasm.rs`, `tools/ggo-emu/web/ggo-emu.js` (lines ~96-105)

**Interfaces (all `#[no_mangle] extern "C"`; `emu: *mut Emulator`; pointers/usize are i32 in wasm):**
- `ggo_abi_version() -> u32` → `ggo_emu_abi::ABI_VERSION`
- `ggo_build_commit_ptr() -> *const u8`, `ggo_build_commit_len() -> usize` → `ggo_emu_core::BUILT_FROM_COMMIT.unwrap_or("")`
- `ggo_emu_new_ex(cart_ptr, cart_len, seed_lo, seed_hi, flags: u32) -> *mut Emulator` — `ggo_emu_new` + `NEW_FLAG_LOG_SINK` sets `peripherals.log_sink = Some(Vec::new())`, `NEW_FLAG_HOST_ASSETS` installs the host card reader. `ggo_emu_new` becomes `ggo_emu_new_ex(.., 0)`.
- `ggo_emu_run_turn(emu, input_mask, ticks_ms) -> u32` returning `STATUS_VSYNC|EXITED|FAULTED|OOM|BUDGET`; stores the Vsync frame number and, on FAULTED/OOM, a reason string. `ggo_emu_run_frame` becomes a wrapper mapping `STATUS_BUDGET` → `STATUS_VSYNC` (0, the browser's "running").
- `ggo_emu_frame_number(emu) -> u32`
- `ggo_emu_fault_detail_ptr(emu) -> *const u8`, `ggo_emu_fault_detail_len(emu) -> usize`. Text EXACTLY: OOM → `format!("out of memory: cart accessed {addr:#010x} past its {}-byte RAM arena at pc={pc:#010x}", plan.arena_len)`; other traps → `format!("cpu fault: {trap:?}")`; full-system fault → `"full-system fault"`.
- `ggo_emu_info_json(emu) -> *const u8`, `ggo_emu_info_json_len(emu) -> usize`: cart mode JSON `{"title":"…","save_bytes":N,"arena_start":N,"arena_end":N,"arena_len":N,"vram_pool":N,"ram_pool":N,"body_truncated":bool}`; `arena_start`/`arena_end` are OFFSETS into psram (`ARENA_BASE - PSRAM_BASE`, `plan.arena_end() - PSRAM_BASE`); pools are `plan.vram_pool.1`/`plan.ram_pool.1`. Escape the title with a local copy of `perfsim.rs`'s `json_escape` (or make that one `pub(crate)`→`pub` and reuse). Full-system: `{}`.
- `ggo_emu_psram_ptr(emu) -> *mut u8`, `ggo_emu_psram_len(emu) -> usize` (cart mode: `mmu.psram`; else null/0).
- `ggo_emu_take_uart`: the `Cart` arm now returns `c.peripherals.take_log()` (empty when no log sink).
- `ggo_emu_uart_inject(emu, ptr, len)`; `ggo_emu_take_comm(emu) -> *const u8` + `ggo_emu_comm_len(emu) -> usize` (staged in a new `comm_buf: Vec<u8>` field).
- `ggo_emu_audio_copy_since(emu, cursor: u64) -> u64` staging into `audio_buf: Vec<i16>` (cart mode: `apu.copy_since`; full-system: returns `cursor`, empty); `ggo_emu_audio_staged_ptr(emu) -> *const i16`, `ggo_emu_audio_staged_len(emu) -> usize` (element count).
- `ggo_emu_ppu_snapshot(emu) -> *const u8`, `ggo_emu_ppu_snapshot_len(emu) -> usize` (`ppu.snapshot()` then `encode` into `snapshot_buf: Vec<u8>`).
- `ggo_emu_perf_frames(emu) -> u32` (`perf.frames.len()`).
- `ggo_emu_save_write(emu, ptr, len)`: copies `min(len, save.len())` bytes into the save region (used to load a save file).
- Standalone APU (`struct ApuHandle { apu: Apu, staged: Vec<i16> }`): `ggo_apu_new() -> *mut ApuHandle`, `ggo_apu_free`, `ggo_apu_queue_samples(apu, vram_off: u32, ptr, len) -> i32`, `ggo_apu_play_sample(apu, ch, start_off, end_off, loop_off, step_vol, adsr) -> i32`, `ggo_apu_run_frame(apu)`, `ggo_apu_copy_since(apu, cursor: u64) -> u64`, `ggo_apu_staged_ptr(apu) -> *const i16`, `ggo_apu_staged_len(apu) -> usize`.
- Host import: `env.ggo_host_read_asset(path_ptr, path_len, dst_ptr, dst_cap) -> i64` (spec "Asset reads").

- [ ] **Step 1: Host import + reader**

```rust
#[link(wasm_import_module = "env")]
extern "C" {
    /// Host-side card-directory read (see the zed host's `ggo_emu_wasm`).
    /// Returns -1 when missing/rejected, else the file's full length; copies
    /// the bytes to `dst` only when they fit in `dst_cap`.
    fn ggo_host_read_asset(path_ptr: *const u8, path_len: usize, dst_ptr: *mut u8, dst_cap: usize) -> i64;
}

fn host_card_read(path: &str) -> Option<Vec<u8>> {
    // SAFETY: the host only reads `path_len` bytes at `path_ptr` and writes at
    // most `dst_cap` bytes at `dst_ptr`, both of which we own for the call.
    let len = unsafe { ggo_host_read_asset(path.as_ptr(), path.len(), core::ptr::null_mut(), 0) };
    let len = usize::try_from(len).ok()?;
    let mut bytes = vec![0u8; len];
    let copied = unsafe { ggo_host_read_asset(path.as_ptr(), path.len(), bytes.as_mut_ptr(), bytes.len()) };
    (usize::try_from(copied).ok()? == len).then_some(bytes)
}
```
In `ggo_emu_new_ex`, when `flags & NEW_FLAG_HOST_ASSETS != 0`: `peripherals.assets.set_card_reader(Box::new(host_card_read));`.

- [ ] **Step 2: Implement every export listed above**, following the file's existing style (doc comment + `# Safety` section per export, `emu.as_ref()/as_mut()` null handling, staging buffers owned by `Emulator`). Refactor `ggo_emu_run_frame`'s body into `ggo_emu_run_turn`; `Vsync(number)` stores `frame_number = number` and blits as today; `Budget` returns `STATUS_BUDGET`; the full-system arm returns `STATUS_VSYNC` for its non-fault outcomes. Store the plan-derived info needed by `info_json` on `CartEmu` at construction (title, save_bytes, `body_truncated = !mmu.load_cart_body(..)`), replacing the current `let _ = mmu.load_cart_body(...)`.

- [ ] **Step 3: Browser import stub** — in `web/ggo-emu.js`, every `instantiate`/`instantiateStreaming` call currently passes `{}` as imports. Define once near the top of the loader:

```js
// The zed host reads card-dir assets through this import; the browser has
// no card directory, so every lookup is a miss.
const IMPORTS = { env: { ggo_host_read_asset: () => -1n } };
```
and pass `IMPORTS` instead of `{}` at all three call sites (i64 return → BigInt `-1n`).

- [ ] **Step 4: Verify exports exist.** Build, then check with wasmparser-free tooling:

```bash
cd /home/clay/projects/ggo/tools && nice -n 19 ionice -c2 -n7 cargo build -p ggo-emu --lib --release --target wasm32-unknown-unknown \
  && python3 - <<'EOF'
import re
data = open('target/wasm32-unknown-unknown/release/ggo_emu.wasm','rb').read()
need = """ggo_abi_version ggo_build_commit_ptr ggo_build_commit_len ggo_emu_new_ex ggo_emu_run_turn
ggo_emu_frame_number ggo_emu_fault_detail_ptr ggo_emu_fault_detail_len ggo_emu_info_json ggo_emu_info_json_len
ggo_emu_psram_ptr ggo_emu_psram_len ggo_emu_uart_inject ggo_emu_take_comm ggo_emu_comm_len
ggo_emu_audio_copy_since ggo_emu_audio_staged_ptr ggo_emu_audio_staged_len ggo_emu_ppu_snapshot
ggo_emu_ppu_snapshot_len ggo_emu_perf_frames ggo_emu_save_write ggo_apu_new ggo_apu_free
ggo_apu_queue_samples ggo_apu_play_sample ggo_apu_run_frame ggo_apu_copy_since ggo_apu_staged_ptr
ggo_apu_staged_len ggo_host_read_asset""".split()
missing = [n for n in need if n.encode() not in data]
print("missing:", missing); raise SystemExit(1 if missing else 0)
EOF
```
Expected: `missing: []`. Behavioural tests of these exports happen in zed Task 6 (the ggo repo has no wasm runtime). If `tools/ggo-emu/web/node_modules` exists, also run the web tests (`cd tools/ggo-emu/web && npx playwright test`); otherwise report them as skipped.

- [ ] **Step 5: Commit**

```bash
cd /home/clay/projects/ggo/tools && nice -n 19 ionice -c2 -n7 cargo test -p ggo-emu \
  && git add -A tools/ggo-emu && git commit -m "ggo-emu wasm: host ABI v1 for the zed emulator runtime"
```

---

### Task 6: zed crate `ggo_emu_wasm` — host (`LoadedEmulator`, `WasmEmu`, `WasmApu`) + fixtures + bundled module

**Files:**
- Create: `crates/ggo/emu_wasm/Cargo.toml`, `crates/ggo/emu_wasm/src/ggo_emu_wasm.rs`, `crates/ggo/emu_wasm/src/fixture.rs`, `crates/ggo/emu_wasm/bundled/ggo_emu.wasm`, `script/update-bundled-ggo-emu`
- Modify: `Cargo.toml` (workspace `members` + `[workspace.dependencies]`: `ggo_emu_wasm = { path = "crates/ggo/emu_wasm" }`, `ggo-emu-abi = { path = "../ggo/tools/ggo-emu-abi" }`, `ggo-savefile = { path = "../ggo/tools/ggo-savefile" }`), `crates/ggo/emu_panel/src/drive.rs` (fixture module moves out; leave `pub use ggo_emu_wasm::fixture;` under the same cfg)

**Interfaces:**
- Consumes: Task 5 exports; `ggo_emu_abi::{ABI_VERSION, abi_major, abi_minor, STATUS_*, NEW_FLAG_*, PpuSnapshot, SCREEN_WIDTH, SCREEN_HEIGHT}`.
- Produces:

```rust
pub const REQUIRED_ABI_MAJOR: u16 = 1;
pub const MIN_ABI_MINOR: u16 = 0;
pub static BUNDLED_WASM: &[u8] = include_bytes!("../bundled/ggo_emu.wasm");

pub fn engine() -> anyhow::Result<&'static wasmtime::Engine>; // epoch interruption on, ticker thread started once

pub struct LoadedEmulator {
    pub label: SharedString,
    pub abi_version: u32,
    pub build_commit: Option<String>,
    // module: wasmtime::Module (private)
}
impl LoadedEmulator {
    pub fn compile(bytes: &[u8], label: impl Into<SharedString>) -> anyhow::Result<LoadedEmulator>;
    pub fn start_cart(&self, cart: &[u8], seed: u64, card_dir: Option<PathBuf>) -> anyhow::Result<WasmEmu>;
    pub fn new_apu(&self) -> anyhow::Result<WasmApu>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEvent { Vsync(u32), Budget, Exit(i32), Fault(String) }

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct CartInfo { pub title: String, pub save_bytes: usize, pub arena_start: usize, pub arena_end: usize,
    pub arena_len: usize, pub vram_pool: usize, pub ram_pool: usize, pub body_truncated: bool }

pub struct WasmEmu { /* store, instance, memory, typed funcs, emu handle, info */ }
impl WasmEmu {
    pub fn info(&self) -> &CartInfo;
    pub fn run_turn(&mut self, input: u32, ticks_ms: u32) -> TurnEvent; // wasm trap → Fault("emulator trapped: {e}")
    pub fn framebuffer_bgra(&mut self) -> anyhow::Result<Vec<u8>>;
    pub fn take_log(&mut self) -> anyhow::Result<Vec<u8>>;
    pub fn uart_inject(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
    pub fn take_comm(&mut self) -> anyhow::Result<Vec<u8>>;
    pub fn audio_copy_since(&mut self, cursor: u64, out: &mut Vec<i16>) -> anyhow::Result<u64>; // appends like Apu::copy_since
    pub fn ppu_snapshot(&mut self) -> anyhow::Result<PpuSnapshot>;
    pub fn with_arena<R>(&mut self, f: impl FnOnce(&mut [u8]) -> R) -> anyhow::Result<R>;
    pub fn save_dirty(&mut self) -> anyhow::Result<bool>;
    pub fn save_bytes(&mut self) -> anyhow::Result<Vec<u8>>;
    pub fn write_save(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
    pub fn clear_save_dirty(&mut self) -> anyhow::Result<()>;
    pub fn perf_json(&mut self) -> anyhow::Result<String>;
    pub fn perf_frames(&mut self) -> anyhow::Result<u64>;
}
pub struct WasmApu;
impl WasmApu {
    pub fn queue_samples(&mut self, vram_off: u32, bytes: &[u8]) -> anyhow::Result<i32>;
    pub fn play_sample(&mut self, ch: u32, start_off: u32, end_off: u32, loop_off: u32, step_vol: u32, adsr: u32) -> anyhow::Result<i32>;
    pub fn run_frame(&mut self) -> anyhow::Result<()>;
    pub fn copy_since(&mut self, cursor: u64, out: &mut Vec<i16>) -> anyhow::Result<u64>;
}
pub mod fixture; // #[cfg(any(test, feature = "test-support"))]: green_screen_cart, saving_cart, logging_cart, comm_echo_cart, overrun_cart
```

- [ ] **Step 1: Bundling script + first bundled module**

`script/update-bundled-ggo-emu` (chmod +x):
```bash
#!/usr/bin/env bash
# Rebuild ggo_emu.wasm from the sibling ggo checkout and bundle it into zed.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
ggo="${GGO_REPO:-$here/../ggo}"
(cd "$ggo/tools" && nice -n 19 ionice -c2 -n7 cargo build -p ggo-emu --lib --release --target wasm32-unknown-unknown)
mkdir -p "$here/crates/ggo/emu_wasm/bundled"
cp "$ggo/tools/target/wasm32-unknown-unknown/release/ggo_emu.wasm" "$here/crates/ggo/emu_wasm/bundled/ggo_emu.wasm"
echo "bundled ggo_emu.wasm from $(git -C "$ggo" rev-parse --short HEAD)"
```
Run it.

- [ ] **Step 2: Crate scaffold** — `crates/ggo/emu_wasm/Cargo.toml`:
```toml
[package]
name = "ggo_emu_wasm"
version = "0.1.0"
edition.workspace = true
publish.workspace = true
license = "GPL-3.0-or-later"

[lints]
workspace = true

[lib]
path = "src/ggo_emu_wasm.rs"

[features]
test-support = ["dep:ggo-emu-core", "dep:gemdrop-sdk"]

[dependencies]
anyhow.workspace = true
ggo-emu-abi.workspace = true
gpui.workspace = true
serde.workspace = true
serde_json.workspace = true
wasmtime.workspace = true
ggo-emu-core = { workspace = true, optional = true } # GGO -- fixture carts only (header consts, crc32, syscall numbers)
gemdrop-sdk = { workspace = true, optional = true }

[dev-dependencies]
ggo-emu-core.workspace = true
gemdrop-sdk.workspace = true
tempfile.workspace = true
```
Add `"crates/ggo/emu_wasm"` to workspace `members` next to the other `crates/ggo/*` entries.

- [ ] **Step 3: Move fixtures.** Cut `pub mod fixture { ... }` from `crates/ggo/emu_panel/src/drive.rs` into `crates/ggo/emu_wasm/src/fixture.rs` (drop the `mod fixture {` wrapper; keep its contents and doc). In `ggo_emu_wasm.rs`: `#[cfg(any(test, feature = "test-support"))] pub mod fixture;`. In `drive.rs`, where the module was: `#[cfg(any(test, feature = "test-support"))] pub use ggo_emu_wasm::fixture;`. In `emu_panel/Cargo.toml`: add `ggo_emu_wasm.workspace = true`; `test-support = ["ggo_emu_wasm/test-support"]`; add `ggo_emu_wasm = { workspace = true, features = ["test-support"] }` to dev-deps.

- [ ] **Step 4: Failing tests** (bottom of `ggo_emu_wasm.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn loaded() -> LoadedEmulator {
        LoadedEmulator::compile(BUNDLED_WASM, "bundled").expect("bundled module compiles")
    }

    fn run_until_vsyncs(emu: &mut WasmEmu, count: usize) -> Vec<TurnEvent> {
        let mut vsyncs = Vec::new();
        for turn in 0..count * 20 {
            match emu.run_turn(0, turn as u32 * 16) {
                event @ TurnEvent::Vsync(_) => { vsyncs.push(event); if vsyncs.len() == count { break } }
                TurnEvent::Budget => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        vsyncs
    }

    #[test]
    fn bundled_module_reports_a_compatible_abi() {
        let emulator = loaded();
        assert_eq!(ggo_emu_abi::abi_major(emulator.abi_version), REQUIRED_ABI_MAJOR);
    }

    #[test]
    fn a_module_with_the_wrong_abi_major_is_rejected() {
        // Minimal module exporting only ggo_abi_version() -> 2<<16, as a
        // hand-written binary (magic, type, func, export, code sections).
        let bytes = wat_free_abi_module(2 << 16);
        let error = LoadedEmulator::compile(&bytes, "bad").err().expect("rejected");
        assert!(error.to_string().contains("ABI"), "{error}");
    }

    #[test]
    fn the_green_cart_presents_green_frames() {
        let mut emu = loaded().start_cart(&fixture::green_screen_cart(), 1, None).expect("starts");
        assert_eq!(run_until_vsyncs(&mut emu, 3).len(), 3);
        let bgra = emu.framebuffer_bgra().expect("framebuffer");
        assert_eq!(bgra.len(), ggo_emu_abi::SCREEN_PIXELS * 4);
        assert_eq!(&bgra[..4], &[0x00, 0xFF, 0x00, 0xFF]); // pure green, BGRA
        assert_eq!(emu.ppu_snapshot().expect("snapshot").tiles.len(), ggo_emu_abi::VRAM_TILE_CAP * ggo_emu_abi::TILE_BYTES);
    }

    #[test]
    fn the_overrun_cart_faults_out_of_memory() {
        let mut emu = loaded().start_cart(&fixture::overrun_cart(), 1, None).expect("starts");
        let event = (0..100).map(|turn| emu.run_turn(0, turn)).find(|event| !matches!(event, TurnEvent::Budget | TurnEvent::Vsync(_)));
        assert!(matches!(&event, Some(TurnEvent::Fault(reason)) if reason.starts_with("out of memory")), "{event:?}");
    }

    #[test]
    fn the_logging_cart_log_reaches_take_log() {
        let mut emu = loaded().start_cart(&fixture::logging_cart(), 1, None).expect("starts");
        run_until_vsyncs(&mut emu, 1);
        assert!(!emu.take_log().expect("log").is_empty());
    }

    #[test]
    fn card_dir_reads_are_confined_to_the_card_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("inside.bin"), [7u8; 4]).expect("write");
        assert_eq!(read_card_file(dir.path(), "inside.bin"), Some(vec![7u8; 4]));
        assert_eq!(read_card_file(dir.path(), "../outside.bin"), None);
        assert_eq!(read_card_file(dir.path(), "/etc/passwd"), None);
    }

    #[test]
    fn the_standalone_apu_mixes_a_queued_clip() {
        let mut apu = loaded().new_apu().expect("apu");
        assert!(apu.queue_samples(0, &[0x11u8; 64]).expect("queue") > 0);
        assert!(apu.play_sample(0, 0, 64, ggo_emu_abi::ONE_SHOT, 0x1000 | (0xFF << 16) | (0xFF << 24), 0).expect("play") >= 0);
        apu.run_frame().expect("frame");
        let mut out = Vec::new();
        assert!(apu.copy_since(0, &mut out).expect("copy") > 0);
        assert!(!out.is_empty());
    }
}
```
`wat_free_abi_module(version: u32) -> Vec<u8>` is a `#[cfg(test)]` helper that emits the bytes of a module with one function `() -> i32` returning `version as i32` exported as `ggo_abi_version` (magic `\0asm`, version 1; type section `60 00 01 7f`; function section; export section; code section `41 <sleb128 version> 0b`). If the green fixture's colour is not pure green, assert the colour the existing drive test asserts for it (grep `green` in `drive.rs` tests). If the logging cart needs more frames to log, raise the count.

- [ ] **Step 5:** `nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_wasm` → FAIL (types missing).

- [ ] **Step 6: Implement.** Key parts (fill the remaining methods in the same pattern):

```rust
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use gpui::SharedString;
use wasmtime::{Caller, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

/// Epoch ticks per `run_turn` before a wedged module is trapped. The ticker
/// runs every 100 ms, so this is ~5 s -- far past any real turn (the
/// in-module 5M-instruction budget ends those), short enough that a
/// hung module cannot freeze a session for good.
const TURN_DEADLINE_TICKS: u64 = 50;
const EPOCH_INTERVAL: Duration = Duration::from_millis(100);

pub fn engine() -> Result<&'static Engine> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    let engine = ENGINE.get_or_init(|| {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|error| error.to_string())?;
        let weak = engine.weak();
        std::thread::Builder::new()
            .name("ggo-emu-epoch".into())
            .spawn(move || {
                while let Some(engine) = weak.upgrade() {
                    engine.increment_epoch();
                    drop(engine);
                    std::thread::sleep(EPOCH_INTERVAL);
                }
            })
            .map(drop)
            .unwrap_or_else(|error| log::error!("ggo-emu-epoch thread: {error}"));
        Ok(engine)
    });
    engine.as_ref().map_err(|error| anyhow!("wasmtime engine: {error}"))
}

struct HostState {
    card_dir: Option<PathBuf>,
}

/// The guest's asset lookups resolve only inside the cart's own directory,
/// because the module may come from the network.
fn read_card_file(card_dir: &Path, path: &str) -> Option<Vec<u8>> {
    let relative = Path::new(path);
    if relative.is_absolute() || !relative.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    std::fs::read(card_dir.join(relative)).ok()
}

fn linker() -> Result<Linker<HostState>> {
    let mut linker = Linker::new(engine()?);
    linker.func_wrap(
        "env",
        "ggo_host_read_asset",
        |mut caller: Caller<'_, HostState>, path_ptr: u32, path_len: u32, dst_ptr: u32, dst_cap: u32| -> i64 {
            let Some(memory) = caller.get_export("memory").and_then(|export| export.into_memory()) else { return -1 };
            let Some(card_dir) = caller.data().card_dir.clone() else { return -1 };
            let start = path_ptr as usize;
            let Some(path) = memory.data(&caller).get(start..start + path_len as usize).map(|raw| String::from_utf8_lossy(raw).into_owned()) else { return -1 };
            let Some(bytes) = read_card_file(&card_dir, &path) else { return -1 };
            if bytes.len() <= dst_cap as usize && memory.write(&mut caller, dst_ptr as usize, &bytes).is_err() {
                return -1;
            }
            bytes.len() as i64
        },
    )?;
    Ok(linker)
}
```
`LoadedEmulator::compile`: `Module::new(engine()?, bytes)`; instantiate a throwaway `Store` (with `set_epoch_deadline(TURN_DEADLINE_TICKS)`) to call `ggo_abi_version` and read the build commit (`ggo_build_commit_ptr/len` → `memory.data(&store).get(ptr..ptr+len)`; empty → `None`); `ensure!(abi_major(v) == REQUIRED_ABI_MAJOR && abi_minor(v) >= MIN_ABI_MINOR, "emulator module ABI {}.{} is incompatible (need {REQUIRED_ABI_MAJOR}.{MIN_ABI_MINOR}+)", ...)`. A missing `ggo_abi_version` export is also an ABI error: `.context("emulator module has no ggo_abi_version export (pre-ABI build?)")`.

`start_cart`: new `Store::new(engine()?, HostState { card_dir })`, instantiate, `ggo_alloc(len)`, `memory.write`, `ggo_emu_new_ex(ptr, len, seed as u32, (seed >> 32) as u32, NEW_FLAG_LOG_SINK | NEW_FLAG_HOST_ASSETS)`, `ggo_free(ptr, len)`, bail if handle 0 (`"cart failed to parse"` — the drive loop will prefix `cart: `), then read + `serde_json::from_slice::<CartInfo>` the info JSON. Cache every `TypedFunc` in the struct at construction (`instance.get_typed_func::<(u32, u32, u32), u32>(&mut store, "ggo_emu_run_turn")?` etc.) so a missing export fails at start, not mid-run.

`run_turn`: `store.set_epoch_deadline(TURN_DEADLINE_TICKS)` then call; map status: VSYNC → `Vsync(frame_number)`, BUDGET → `Budget`, EXITED → `Exit(exit_code)`, FAULTED|OOM → `Fault(fault_detail)`; unknown status → `Fault(format!("emulator returned unknown status {status}"))`; `Err(trap)` → `Fault(format!("emulator trapped: {trap}"))`.

Reading staged buffers — one private helper used by every ptr/len pair:
```rust
fn read_bytes(&mut self, ptr: u32, len: usize) -> Result<Vec<u8>> {
    let start = ptr as usize;
    self.memory
        .data(&self.store)
        .get(start..start + len)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| anyhow!("emulator returned an out-of-bounds buffer {start:#x}+{len}"))
}
```
i16 buffers: read `len * 2` bytes and `chunks_exact(2).map(|pair| i16::from_le_bytes([pair[0], pair[1]]))`. `framebuffer_bgra`: read `SCREEN_PIXELS * 4` RGBA bytes, then swap bytes 0/2 per pixel inline with `chunks_exact_mut(4).for_each(|pixel| pixel.swap(0, 2))` (not `ggo_common::rgba_to_bgra` — that would make this crate depend on `ggo_common`). `with_arena`: `let psram = psram_ptr as usize; let range = psram + info.arena_start..psram + info.arena_end; let data = self.memory.data_mut(&mut self.store); let arena = data.get_mut(range).context("arena outside linear memory")?; Ok(f(arena))`. Re-fetch `data()` on every call; never hold a slice across a guest call (memory can grow).

`WasmApu`: own `Store<HostState { card_dir: None }>` + instance, handle from `ggo_apu_new`, same helpers. `Drop` for `WasmEmu`/`WasmApu` calls `ggo_emu_free`/`ggo_apu_free` and `.log_err()`s any failure (add `util.workspace = true` for `ResultExt`, or `if let Err(e) = ... { log::error!(...) }` — add `log.workspace = true`).

- [ ] **Step 7: Verify and commit**

```bash
cd /home/clay/projects/zed && nice -n 19 ionice -c2 -n7 ./script/clippy -p ggo_emu_wasm \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_wasm \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_panel --lib drive \
  && git add Cargo.toml Cargo.lock script/update-bundled-ggo-emu crates/ggo/emu_wasm crates/ggo/emu_panel \
  && git commit -m "ggo: host the emulator as a wasmtime module (ggo_emu_wasm)"
```
(`drive` tests still run natively here — only the fixture moved.)

---

### Task 7: Emulator sources

**Files:**
- Create: `crates/ggo/emu_wasm/src/sources.rs`
- Modify: `crates/ggo/emu_wasm/Cargo.toml` (+ `futures`, `http_client`, `fs`, `serde`), `src/ggo_emu_wasm.rs` (`pub mod sources;`)

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmulatorVersion { pub id: String, pub label: String, pub download_url: Option<String>, pub prerelease: bool }

pub trait EmulatorSource: Send + Sync {
    fn describe(&self) -> SharedString;
    fn list_versions(&self) -> BoxFuture<'static, Result<Vec<EmulatorVersion>>>;
    fn fetch(&self, version: &EmulatorVersion) -> BoxFuture<'static, Result<Arc<[u8]>>>;
}

pub struct BundledSource;
pub struct LocalSource { pub path: PathBuf, pub fs: Arc<dyn Fs> }
pub struct HttpSource { pub url: String, pub http: Arc<dyn HttpClient> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgejoConfig { pub base_url: String, pub owner: String, pub repo: String,
    #[serde(default = "default_asset")] pub asset: String,   // "ggo_emu.wasm"
    #[serde(default = "default_tag")] pub tag: String,       // "latest"
    #[serde(default)] pub token: Option<String> }
pub struct ForgejoSource { pub config: ForgejoConfig, pub http: Arc<dyn HttpClient> }
impl ForgejoSource { pub fn resolve(&self, versions: &[EmulatorVersion]) -> Result<EmulatorVersion>; } // applies `tag`
```
Version ids: bundled `"bundled"`; local = the path string; http = the URL; forgejo = `"{base_url}/{owner}/{repo}@{tag_name}"`.

- [ ] **Step 1: Failing tests** (in `sources.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use http_client::FakeHttpClient;

    const RELEASES: &str = r#"[
      {"tag_name":"v0.3.0-rc1","draft":false,"prerelease":true,
       "assets":[{"name":"ggo_emu.wasm","browser_download_url":"https://git.example/dl/rc1.wasm"}]},
      {"tag_name":"v0.2.0","draft":false,"prerelease":false,
       "assets":[{"name":"ggo_emu.wasm","browser_download_url":"https://git.example/dl/v020.wasm"}]},
      {"tag_name":"v0.1.9","draft":true,"prerelease":false,
       "assets":[{"name":"ggo_emu.wasm","browser_download_url":"https://git.example/dl/draft.wasm"}]},
      {"tag_name":"v0.1.0","draft":false,"prerelease":false,
       "assets":[{"name":"other.bin","browser_download_url":"https://git.example/dl/other.bin"}]}
    ]"#;

    fn forgejo(tag: &str) -> ForgejoSource {
        let http = FakeHttpClient::create(|request| async move {
            let uri = request.uri().to_string();
            let body = match uri.as_str() {
                "https://git.example/api/v1/repos/gemdrop/ggo/releases?limit=50" => RELEASES.as_bytes().to_vec(),
                "https://git.example/dl/v020.wasm" => b"\0asmV020".to_vec(),
                _ => return Ok(http_client::Response::builder().status(404).body(Default::default())?),
            };
            Ok(http_client::Response::builder().status(200).body(body.into())?)
        });
        ForgejoSource {
            config: ForgejoConfig { base_url: "https://git.example".into(), owner: "gemdrop".into(), repo: "ggo".into(),
                asset: "ggo_emu.wasm".into(), tag: tag.into(), token: None },
            http,
        }
    }

    #[test]
    fn forgejo_lists_non_draft_releases_that_carry_the_asset() {
        let versions = futures::executor::block_on(forgejo("latest").list_versions()).expect("list");
        let labels: Vec<_> = versions.iter().map(|v| v.label.as_str()).collect();
        assert_eq!(labels, ["v0.3.0-rc1 (pre-release)", "v0.2.0"]);
    }

    #[test]
    fn latest_skips_pre_releases_and_fetches_the_asset() {
        let source = forgejo("latest");
        let versions = futures::executor::block_on(source.list_versions()).expect("list");
        let version = source.resolve(&versions).expect("resolve");
        assert_eq!(version.id, "https://git.example/gemdrop/ggo@v0.2.0");
        let bytes = futures::executor::block_on(source.fetch(&version)).expect("fetch");
        assert_eq!(&bytes[..], b"\0asmV020");
    }

    #[test]
    fn a_pinned_tag_that_does_not_exist_is_an_error() {
        let source = forgejo("v9.9.9");
        let versions = futures::executor::block_on(source.list_versions()).expect("list");
        assert!(source.resolve(&versions).is_err());
    }

    #[test]
    fn http_source_fetches_its_url_and_reports_http_errors() {
        let http = FakeHttpClient::create(|request| async move {
            let status = if request.uri().path() == "/ok.wasm" { 200 } else { 500 };
            Ok(http_client::Response::builder().status(status).body(b"\0asm".to_vec().into())?)
        });
        let ok = HttpSource { url: "https://host/ok.wasm".into(), http: http.clone() };
        let version = futures::executor::block_on(ok.list_versions()).expect("list").remove(0);
        assert_eq!(&futures::executor::block_on(ok.fetch(&version)).expect("fetch")[..], b"\0asm");
        let bad = HttpSource { url: "https://host/bad.wasm".into(), http };
        let version = futures::executor::block_on(bad.list_versions()).expect("list").remove(0);
        assert!(futures::executor::block_on(bad.fetch(&version)).is_err());
    }

    #[gpui::test]
    async fn local_source_reads_its_file(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_file("/work/ggo_emu.wasm", b"\0asmLOCAL".to_vec()).await;
        let source = LocalSource { path: "/work/ggo_emu.wasm".into(), fs };
        let version = source.list_versions().await.expect("list").remove(0);
        assert_eq!(&source.fetch(&version).await.expect("fetch")[..], b"\0asmLOCAL");
    }
}
```
If `FakeHttpClient::create`'s closure/body types differ (check `crates/http_client/src/http_client.rs:431` and an existing caller via `grep -rn "FakeHttpClient::create" crates | head`), adapt the test's response construction to match; keep the assertions.

- [ ] **Step 2:** `cargo test -p ggo_emu_wasm sources` → FAIL.
- [ ] **Step 3: Implement.** Shared GET helper:

```rust
async fn get_bytes(http: Arc<dyn HttpClient>, url: String, token: Option<String>) -> Result<Vec<u8>> {
    let mut request = http_client::Request::builder().method(http_client::Method::GET).uri(&url);
    if let Some(token) = token {
        request = request.header("Authorization", format!("token {token}"));
    }
    let mut response = http
        .send(request.follow_redirects(http_client::RedirectPolicy::FollowAll).body(AsyncBody::empty())?)
        .await
        .with_context(|| format!("GET {url}"))?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    ensure!(response.status().is_success(), "GET {url}: HTTP {}", response.status());
    Ok(body)
}
```
(check `http_client` for the exact `follow_redirects`/`RedirectPolicy` builder extension name — grep `RedirectPolicy` in `crates/http_client`.)
Forgejo: GET `{base_url}/api/v1/repos/{owner}/{repo}/releases?limit=50` (trim a trailing `/` from `base_url`), deserialize `Vec<Release { tag_name: String, draft: bool, prerelease: bool, assets: Vec<Asset { name: String, browser_download_url: String }> }>`, keep `!draft` releases with an asset named `config.asset`, label `tag_name` + `" (pre-release)"` when prerelease. `resolve`: `tag == "latest"` → first version with `!prerelease`; otherwise the version whose tag equals `tag`; else `bail!("no release {tag} with asset {asset} in {owner}/{repo}")`. `fetch` GETs `download_url` with the token. `BundledSource`: one version `{id:"bundled", label:"Bundled", download_url:None, prerelease:false}`, `fetch` → `Arc::from(BUNDLED_WASM)`. `LocalSource::fetch` → `fs.load_bytes(&path)`. Each source's `describe`: `"Bundled"`, `"Local: {path}"`, `"URL: {url}"`, `"Forgejo: {owner}/{repo}@{tag}"`.
- [ ] **Step 4: Verify and commit**

```bash
cd /home/clay/projects/zed && nice -n 19 ionice -c2 -n7 ./script/clippy -p ggo_emu_wasm \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_wasm \
  && git add Cargo.lock crates/ggo/emu_wasm && git commit -m "ggo: emulator module sources (bundled, local, URL, Forgejo)"
```

---

### Task 8: `EmuRuntime` global — config file, cache, load, swap

**Files:**
- Create: `crates/ggo/emu_wasm/src/runtime.rs`
- Modify: `crates/ggo/emu_wasm/Cargo.toml` (+ `paths`, `sha2`, `log`, `util`), `src/ggo_emu_wasm.rs` (`pub mod runtime; pub use runtime::*;`), GGO init site in `crates/zed/src/main.rs` (find where `ggo_emu_panel::init` or similar is called: `grep -n "ggo_" crates/zed/src/main.rs crates/zed/src/zed.rs`)

**Interfaces:**
- Consumes: Tasks 6-7.
- Produces:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceConfig { #[default] Bundled, Path(PathBuf), Url(String), Forgejo(ForgejoConfig) }

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmulatorConfig { #[serde(default)] pub source: SourceConfig }

pub fn config_path() -> PathBuf;                      // paths::home_dir().join(".ggo/emulator.json")
pub fn source_for(config: &SourceConfig, fs: Arc<dyn Fs>, http: Arc<dyn HttpClient>) -> Arc<dyn EmulatorSource>;

pub struct EmulatorChanged;                            // event
pub struct EmuRuntime { /* current, status, config, tasks */ }
impl EventEmitter<EmulatorChanged> for EmuRuntime {}
impl EmuRuntime {
    pub fn init(fs: Arc<dyn Fs>, http: Arc<dyn HttpClient>, cx: &mut App);   // registers global, starts watch + first load
    pub fn init_with_config_path(fs: Arc<dyn Fs>, http: Arc<dyn HttpClient>, config_path: PathBuf, cache_dir: PathBuf, cx: &mut App); // tests
    pub fn global(cx: &App) -> Option<Entity<EmuRuntime>>;
    pub fn current(&self) -> Option<Arc<LoadedEmulator>>;
    pub fn status(&self) -> RuntimeStatus;             // Loading | Ready | Failed(SharedString)
    pub fn config(&self) -> &EmulatorConfig;
    pub fn source(&self) -> Arc<dyn EmulatorSource>;
    pub fn select(&mut self, config: EmulatorConfig, cx: &mut Context<Self>) -> Task<Result<()>>; // writes config file; the watcher triggers the reload
}
pub fn current_emulator(cx: &App) -> Result<Arc<LoadedEmulator>>; // "emulator module is still loading" / "failed: …"
```

Behaviour:
- `init` reads the config (missing → default; malformed → `Failed("~/.ggo/emulator.json: {error}")` and load Bundled anyway), then `reload`.
- `reload`: background task → `source.list_versions()` → pick (`ForgejoSource::resolve` for forgejo, else first) → `source.fetch(version)`; on success write `cache_dir/<hex sha256(version.id)>.wasm`; on fetch failure read that cache file, else error → `LoadedEmulator::compile(bytes, label)` on `cx.background_spawn` → on the foreground: set `current`, `status = Ready`, `cx.emit(EmulatorChanged)`, `cx.notify()`. On any error: `log::error!`, `status = Failed(msg)`; if `current` is `None`, compile `BUNDLED_WASM` as fallback (and emit). A reload started while another is in flight replaces the stored task (drops = cancels the old one).
- Watchers: `fs.watch(config_path.parent(), 100ms)` → on events touching `config_path`, re-read; if config changed, reload. When the source is `Path(path)`, a second watcher on `path.parent()` → on events touching `path`, reload (dev loop). Store the watch tasks in fields; replace the path watcher when the config changes.
- Cache dir in production: `paths::data_dir().join("ggo-emu")`.

- [ ] **Step 1: Failing tests** (`runtime.rs`, gpui tests with `FakeFs`; `BUNDLED_WASM` is a real module, so use it as the "local file" contents too):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn setup(cx: &mut TestAppContext, config: Option<&str>) -> (Arc<fs::FakeFs>, Entity<EmuRuntime>) {
        let fs = fs::FakeFs::new(cx.executor());
        cx.executor().block(fs.create_dir("/home/.ggo".as_ref())).expect("dir");
        if let Some(config) = config {
            cx.executor().block(fs.insert_file("/home/.ggo/emulator.json", config.as_bytes().to_vec()));
        }
        cx.update(|cx| {
            EmuRuntime::init_with_config_path(fs.clone(), http_client::FakeHttpClient::with_404_response(),
                "/home/.ggo/emulator.json".into(), "/cache".into(), cx)
        });
        cx.run_until_parked();
        let runtime = cx.update(|cx| EmuRuntime::global(cx)).expect("global");
        (fs, runtime)
    }

    #[gpui::test]
    async fn no_config_file_loads_the_bundled_module(cx: &mut TestAppContext) {
        let (_fs, runtime) = setup(cx, None);
        runtime.read_with(cx, |runtime, _| {
            assert_eq!(runtime.status(), RuntimeStatus::Ready);
            assert_eq!(runtime.current().expect("loaded").label.as_ref(), "Bundled");
        });
    }

    #[gpui::test]
    async fn rewriting_a_local_module_swaps_and_emits(cx: &mut TestAppContext) {
        let (fs, runtime) = setup(cx, Some(r#"{"source":{"path":"/work/ggo_emu.wasm"}}"#));
        fs.insert_file("/work/ggo_emu.wasm", BUNDLED_WASM.to_vec()).await;
        let changes = Rc::new(Cell::new(0));
        let _subscription = cx.update(|cx| cx.subscribe(&runtime, {
            let changes = changes.clone();
            move |_, _: &EmulatorChanged, _| changes.set(changes.get() + 1)
        }));
        fs.insert_file("/work/ggo_emu.wasm", BUNDLED_WASM.to_vec()).await; // touch
        cx.run_until_parked();
        assert!(changes.get() >= 1);
        runtime.read_with(cx, |runtime, _| assert!(runtime.current().expect("loaded").label.starts_with("Local")));
    }

    #[gpui::test]
    async fn a_failing_source_keeps_the_previous_module_and_reports(cx: &mut TestAppContext) {
        let (fs, runtime) = setup(cx, None);
        fs.insert_file("/home/.ggo/emulator.json", br#"{"source":{"url":"https://nowhere/ggo_emu.wasm"}}"#.to_vec()).await;
        cx.run_until_parked();
        runtime.read_with(cx, |runtime, _| {
            assert!(matches!(runtime.status(), RuntimeStatus::Failed(_)));
            assert_eq!(runtime.current().expect("kept").label.as_ref(), "Bundled");
        });
    }

    #[gpui::test]
    async fn a_malformed_config_reports_and_falls_back_to_bundled(cx: &mut TestAppContext) {
        let (_fs, runtime) = setup(cx, Some("{not json"));
        runtime.read_with(cx, |runtime, _| {
            assert!(matches!(runtime.status(), RuntimeStatus::Failed(message) if message.contains("emulator.json")));
            assert!(runtime.current().is_some());
        });
    }

    #[test]
    fn config_json_shapes_match_the_spec() {
        let parse = |json: &str| serde_json::from_str::<EmulatorConfig>(json).expect(json).source;
        assert_eq!(parse(r#"{"source":"bundled"}"#), SourceConfig::Bundled);
        assert_eq!(parse(r#"{"source":{"path":"/x.wasm"}}"#), SourceConfig::Path("/x.wasm".into()));
        assert_eq!(parse(r#"{"source":{"url":"https://h/x.wasm"}}"#), SourceConfig::Url("https://h/x.wasm".into()));
        assert!(matches!(parse(r#"{"source":{"forgejo":{"base_url":"https://g","owner":"o","repo":"r"}}}"#),
            SourceConfig::Forgejo(config) if config.tag == "latest" && config.asset == "ggo_emu.wasm"));
    }
}
```
Compiling a real module in a gpui test runs on `background_spawn`; if the test executor doesn't make real progress for CPU work, `cx.run_until_parked()` still drives it (background tasks run on the test dispatcher). If `FakeFs` watch events need `cx.executor().advance_clock(Duration::from_millis(200))` to fire, add it before `run_until_parked`.

- [ ] **Step 2:** `cargo test -p ggo_emu_wasm runtime` → FAIL.
- [ ] **Step 3: Implement** per "Behaviour" above. `struct GlobalEmuRuntime(Entity<EmuRuntime>); impl Global for GlobalEmuRuntime {}`. `LoadedEmulator.label` = the source's `describe()` for bundled/local/url, or `"Forgejo: {owner}/{repo}@{tag_name}"` for forgejo.
- [ ] **Step 4: Wire init.** Call `ggo_emu_wasm::EmuRuntime::init(app_state.fs.clone(), client.http_client(), cx)` next to the other GGO crate inits (find them with `grep -n "ggo" crates/zed/src/main.rs crates/zed/src/zed.rs`); add `ggo_emu_wasm.workspace = true` to `crates/zed/Cargo.toml`.
- [ ] **Step 5: Verify and commit**

```bash
cd /home/clay/projects/zed && nice -n 19 ionice -c2 -n7 ./script/clippy -p ggo_emu_wasm -p zed \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_wasm \
  && git add Cargo.lock crates/ggo/emu_wasm crates/zed && git commit -m "ggo: EmuRuntime loads and swaps the emulator module from ~/.ggo/emulator.json"
```

---

### Task 9: Port `drive.rs` + `link.rs` onto `WasmEmu`

**Files:**
- Modify: `crates/ggo/emu_panel/src/drive.rs`, `src/link.rs`, `src/debug.rs`, `src/audio.rs` (lines 146, 695), `src/ggo_emu_panel.rs` (every `ggo_emu_core::` use; `drive::start` call sites), `src/viewer_run.rs` (line ~448), `src/hardware.rs` (~1505), `crates/ggo/emu_panel/Cargo.toml`

**Interfaces:**
- Consumes: `LoadedEmulator::start_cart`, `WasmEmu` methods, `TurnEvent`, `CartInfo`, `current_emulator(cx)`, `ggo_emu_abi::*`, `ggo_savefile::*`.
- Produces: `drive::start(emulator: Arc<LoadedEmulator>, cart_path: PathBuf, cart: String, audio: Option<AudioStatus>, link: Option<Arc<LinkEndpoint>>) -> (Session, Receiver<Frame>)` (new FIRST parameter; everything else unchanged). `Session::snapshot()` still returns `Option<Arc<PpuSnapshot>>`, now `ggo_emu_abi::PpuSnapshot`. `hardware` env builder takes `emu_commit: Option<String>` from its caller instead of reading `BUILT_FROM_COMMIT`.

- [ ] **Step 1: Retarget the existing tests first.** In `drive.rs` tests, every `start(...)` call gains a first argument `test_emulator()`:

```rust
fn test_emulator() -> Arc<ggo_emu_wasm::LoadedEmulator> {
    static EMULATOR: std::sync::OnceLock<Arc<ggo_emu_wasm::LoadedEmulator>> = std::sync::OnceLock::new();
    EMULATOR
        .get_or_init(|| Arc::new(ggo_emu_wasm::LoadedEmulator::compile(ggo_emu_wasm::BUNDLED_WASM, "bundled").expect("bundled compiles")))
        .clone()
}
```
Put it in `drive::tests_support` (already `pub mod`) so `viewer_run.rs`/`ggo_emu_panel.rs` tests and `ggo_smoke` can reuse it. Tests that build native `Apu`/`Ppu` state directly (`apu_with_one_mixed_frame`, `pump_audio_*`, the `Ppu::new()` tests in `ggo_emu_panel.rs:7232/9676/9983`, `debug.rs:344`) keep using the dev-dep `ggo-emu-core` natively where they only need to fabricate data; convert `Ppu::new()`+`snapshot()` uses to `PpuSnapshot::default()` plus direct field writes where that is simpler. `link.rs` tests that call `dispatch_ecall` on native `Peripherals` are rewritten to run `fixture::comm_echo_cart()` through a `WasmEmu` (`uart_inject` → `run_turn` until Vsync → `take_comm`) or deleted where `the_frame_boundary_pump_carries_the_link_both_ways` in `drive.rs` already covers the same path — keep the pure `pump_inbound` byte-level tests unchanged.

- [ ] **Step 2:** `nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_panel --lib` → FAIL to compile (new `start` signature).

- [ ] **Step 3: Port `run`.** Replace the native setup (cart parse → plan → Mmu → Cpu → sandbox → Peripherals) with:

```rust
let bytes = /* unchanged std::fs::read */;
let seed = /* unchanged SystemTime seed */;
let mut emu = match emulator.start_cart(&bytes, seed, cart_path.parent().map(Path::to_path_buf)) {
    Ok(emu) => emu,
    Err(error) => {
        uart.push_line(format!("[cart load failed] {error}"));
        return RunOutcome { reason: format!("cart: {error}"), is_error: true, perf: None };
    }
};
let info = emu.info().clone();
uart.push_line(format!("[ram] arena {} KiB, vram pool {} KiB, ram pool {} KiB",
    info.arena_len / 1024, info.vram_pool / 1024, info.ram_pool / 1024));
if info.body_truncated {
    uart.push_line("[cart] body is larger than the code window; truncated");
}
```
Save: `let save_file = save_file_for(cart_path, &info.title, info.save_bytes);` then if `Some(path)`: `let mut save = vec![0u8; info.save_bytes]; ggo_savefile::load_save(path, &info.title, &mut save); emu.write_save(&save)`. `flush_save` becomes `(save_file, title, emu: &mut WasmEmu, uart)` using `emu.save_bytes()` → `ggo_savefile::flush_save` → `emu.clear_save_dirty()`; any `WasmEmu` error → `uart.push_line(format!("[save] {error}"))`.

The loop: keep `input_mask` and `ticks_ms` as locals (`let mut input_mask = 0; let mut ticks_ms = 0;`), call `emu.run_turn(input_mask, ticks_ms)`, and where the old code wrote `p.input_mask = …`/`p.set_ticks_ms(…)` assign the locals instead. `uart.push(&p.take_log())` → `match emu.take_log() { Ok(log) => uart.push(&log), Err(error) => break (format!("emulator: {error}"), true) }`. Arms:
- `TurnEvent::Vsync(number)`: `emu.framebuffer_bgra()`, `emu.ppu_snapshot()` into the snapshot slot (`*snapshot.lock() = Some(Arc::new(snap))` — the buffer-reuse trick goes away), tap via `emu.with_arena(|arena| …)`, link via `pump_link(&mut emu, …)`, audio via `emu.audio_copy_since`, save via `emu.save_dirty()`. Any `Err` from these → break with `(format!("emulator: {error}"), true)`.
- `TurnEvent::Budget`: unchanged park + input latch.
- `TurnEvent::Exit(code)`: unchanged text.
- `TurnEvent::Fault(reason)`: `break (reason, true)` — the module already formats both the OOM and cpu-fault wording.
Perf at end: `perf_json: emu.perf_json()`, `frames: emu.perf_frames()`, `cart: info.title` (on error, `perf: None` and push a `[perf] {error}` line).

Tap helpers: `arm_world_tap`/`publish_world_tap` take `arena: &mut [u8]` / `&[u8]` (the closure argument of `with_arena`) instead of `&Mmu`; `find_tap` is unchanged; delete `arena_range` (the module reports the bounds). `pump_audio(apu: &Apu, …)` becomes `pump_audio(emu: &mut WasmEmu, …) -> Result<u64>`; adjust its tests to drive `fixture::green_screen_cart()` or keep the native `Apu` fabrication only if the function under test still takes one — prefer changing the test to a `WasmEmu`.

Replace `drive::rgb565_to_bgra` and its two tests with nothing (the module now hands BGRA); if other code calls it, point that code at `ggo_emu_abi::rgb565_to_argb`.

- [ ] **Step 4: `link.rs`** — `pump_link(emu: &mut WasmEmu, endpoint, reader) -> anyhow::Result<()>`: `for bytes in endpoint.take_outbound() { emu.uart_inject(&bytes)? }`, `let tx = emu.take_comm()?;`, `pump_inbound(&tx, endpoint, reader)`.

- [ ] **Step 5: Call sites.** `ggo_emu_panel.rs` and `viewer_run.rs`: before `drive::start`, `let emulator = match ggo_emu_wasm::current_emulator(cx) { Ok(e) => e, Err(error) => { /* show error the way the pane shows a failed start today */ return } };`. Replace `ggo_emu_core::ppu::*` / `peripherals::*` constants with `ggo_emu_abi::*`, `ggo_emu_core::apu::{RING_LEN, MIX_RATE}` in `audio.rs` with `ggo_emu_abi::*`. `hardware.rs`: add an `emu_commit: Option<String>` parameter to the env builder; its callers pass `ggo_emu_wasm::EmuRuntime::global(cx).and_then(|runtime| runtime.read(cx).current()).and_then(|emulator| emulator.build_commit.clone())`, captured on the foreground before any background hop. `ggo_emu_core::BUILT_FROM_COMMIT` disappears.

- [ ] **Step 6: Cargo.** In `emu_panel/Cargo.toml`: remove `ggo-emu-core.workspace = true` from `[dependencies]`; add `ggo-emu-abi.workspace = true`, `ggo-savefile.workspace = true`; add `ggo-emu-core.workspace = true` under `[dev-dependencies]` (native data fabrication in tests only).

- [ ] **Step 7: Verify and commit**

```bash
cd /home/clay/projects/zed && nice -n 19 ionice -c2 -n7 ./script/clippy -p ggo_emu_panel \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_panel --lib \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo_smoke \
  && git add Cargo.lock crates/ggo && git commit -m "ggo: emulator pane drives the wasm module instead of linking ggo-emu-core"
```
Every pre-existing drive/viewer/panel test must pass. A changed expectation needs a reason in the commit body (e.g. the snapshot slot no longer reuses buffers).

---

### Task 10: Audio preview on `WasmApu`

**Files:**
- Modify: `crates/ggo/audio_panel/src/preview.rs` (~27, 48, 72, 212-270), `crates/ggo/audio_panel/Cargo.toml`, plus the audio panel's `Preview::start` call site(s)

**Interfaces:**
- Consumes: `LoadedEmulator::new_apu`, `WasmApu`, `current_emulator(cx)`, `ggo_emu_abi::{MIX_RATE, ONE_SHOT}`.
- Produces: `Preview::start(spec, looping, status, emulator: Arc<LoadedEmulator>)`.

- [ ] **Step 1:** Update the existing `run_baked` tests to construct a `WasmApu` from the bundled module (same `test_emulator()` pattern as Task 9, defined locally in the test module) and pass it in; they must keep their assertions. Run `cargo test -p ggo_audio_panel` → FAIL.
- [ ] **Step 2:** `run_baked(apu: &mut WasmApu, blob, …)`: `Apu::new()` → the passed `apu`; `queue_samples`/`play_sample`/`run_frame`/`copy_since` → the `WasmApu` methods; on any `Err`, `log::error!("ggo audio preview: {error}")` and return. `Preview::start` creates the apu inside the thread: `let Ok(mut apu) = emulator.new_apu().inspect_err(|error| log::error!(...)) else { done.store(true, …); return };` (`LoadedEmulator` is `Send + Sync`; `WasmApu` is created and dropped on the preview thread). Constants from `ggo_emu_abi`. Call site: fetch `current_emulator(cx)`; on `Err`, show the panel's existing preview-error path, or `log::error!` if there is none.
- [ ] **Step 3:** Cargo: replace `ggo-emu-core` with `ggo-emu-abi` + `ggo_emu_wasm` (+ `ggo_emu_wasm` dev-dep if tests need `BUNDLED_WASM`).
- [ ] **Step 4: Verify and commit**

```bash
cd /home/clay/projects/zed && nice -n 19 ionice -c2 -n7 ./script/clippy -p ggo_audio_panel \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo_audio_panel \
  && git add Cargo.lock crates/ggo/audio_panel && git commit -m "ggo: audio preview runs the wasm module's APU"
```

---

### Task 11: Restart sessions on swap; surface the active version

**Files:**
- Modify: `crates/ggo/emu_panel/src/ggo_emu_panel.rs`, `src/viewer_run.rs`, `crates/ggo/emu_mcp/src/tools.rs` (emu_status ~417, schema ~147)

**Interfaces:**
- Consumes: `EmuRuntime::global`, `EmulatorChanged`, `RuntimeStatus`, `LoadedEmulator.{label, build_commit, abi_version}`.

- [ ] **Step 1: Failing test** in `ggo_emu_panel.rs` tests, modelled on the existing test that starts a run of `fixture::green_screen_cart()` through the panel (grep `green_screen_cart` in that file for the harness): with `EmuRuntime::init_with_config_path` over a `FakeFs`, start a run, note the session's identity (e.g. a run generation counter or `Session` pointer the panel exposes under `test-support`), rewrite the config to `{"source":{"path":"/work/ggo_emu.wasm"}}` with the bundled bytes at that path, `run_until_parked`, assert a NEW session is running the same cart and the old one ended with reason `"emulator module changed"`. Same shape for `viewer_run` (world view restarts its viewer cart).
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement.** In `EmuPanel::new` (and the viewer-run entity's constructor): `if let Some(runtime) = EmuRuntime::global(cx) { self._subscriptions.push(cx.subscribe(&runtime, |this, _, _: &EmulatorChanged, cx| this.restart_for_new_emulator(cx))) }`. `restart_for_new_emulator`: if a session is live, stop it with reason `"emulator module changed"` via the pane's existing stop path (the one that ingests and records the reason), then start the same cart again via the existing start path (which now picks up `current_emulator(cx)`). Paused sessions restart unpaused. No live session → just `cx.notify()`.

  Status: in the pane's header/status row (where the running cart name is rendered), append the active emulator: `label` + short commit (first 8 chars) or, when `RuntimeStatus::Failed(message)`, the message styled as an error. Observe the runtime entity (`cx.observe`) so the row re-renders.

  MCP: `emu_status` JSON gains `"emulator": {"label": …, "commit": …|null, "abi": "1.0", "status": "ready"|"loading"|"failed: …"}`; update the tool description string to mention it. Adjust the existing `emu_status` test's expected JSON.
- [ ] **Step 4: Verify and commit**

```bash
cd /home/clay/projects/zed && nice -n 19 ionice -c2 -n7 ./script/clippy -p ggo_emu_panel -p ggo_emu_mcp \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_panel --lib \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_mcp \
  && git add crates/ggo && git commit -m "ggo: restart emulator sessions when the module changes; show the active version"
```
(Check the MCP crate's real package name in `crates/ggo/emu_mcp/Cargo.toml` before running.)

---

### Task 12: `ggo: select emulator version` picker

**Files:**
- Create: `crates/ggo/emu_panel/src/emulator_picker.rs`
- Modify: `crates/ggo/emu_panel/src/ggo_emu_panel.rs` (action registration in `actions!` at ~119 and `init`)

**Interfaces:**
- Consumes: `EmuRuntime::{config, select, source}`, `sources::*`, `ggo_common::picker_card::{PickerCard, matches_for, reselect_index}`.
- Produces: action `SelectEmulatorVersion` (in the panel's existing `actions!` namespace) opening `EmulatorPicker` modal.

Rows (in order): `Bundled`; `Local file…` (opens `cx.prompt_for_paths` filtered to one file, then selects `Path`); the configured local path if the config is `Path` (re-select = reload); every Forgejo version from `source.list_versions()` when the config is `Forgejo` (label, `(current)` marker for the active one) plus `Forgejo: latest`; the configured URL if `Url`; `Edit ~/.ggo/emulator.json` (opens the file in the workspace, creating it with the current config serialized if absent). Confirm → `runtime.select(new_config)` → dismiss. Forgejo listing loads async; show `preview_placeholder("Loading releases…")` / the error text until it resolves.

- [ ] **Step 1:** Build the modal by copying the structure of the most recent picker-card user (the emerald panel's add-system card: `grep -rn "PickerCard::new" crates/ggo | head`) — same `ModalView` impl, focus handling, `matches_for`/`reselect_index` usage.
- [ ] **Step 2: Test** (gpui, in `emulator_picker.rs`): with a `Forgejo` config against the `RELEASES` fake from Task 7 (make that fixture `pub` under `test-support` in `sources.rs`), open the picker, wait for rows, assert labels `["Bundled", "Local file…", "Forgejo: latest", "v0.3.0-rc1 (pre-release)", "v0.2.0", "Edit ~/.ggo/emulator.json"]`, confirm `v0.2.0`, assert the written config has `"tag":"v0.2.0"`.
- [ ] **Step 3: Verify and commit**

```bash
cd /home/clay/projects/zed && nice -n 19 ionice -c2 -n7 ./script/clippy -p ggo_emu_panel \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_panel --lib emulator_picker \
  && git add crates/ggo && git commit -m "ggo: picker to select the emulator module version"
```

---

### Task 13: Final gate — no runtime `ggo-emu-core`, full test pass

- [ ] **Step 1: Prove the dependency is gone from non-test builds**

```bash
cd /home/clay/projects/zed && nice -n 19 ionice -c2 -n7 cargo tree -p zed -e normal -i ggo-emu-core 2>&1 | head -5
```
Expected: `error: package ID specification \`ggo-emu-core\` did not match any packages` (or equivalent "nothing depends on it" output). If something still depends on it normally, remove that edge.

- [ ] **Step 2: Full GGO suite + clippy**

```bash
cd /home/clay/projects/zed && nice -n 19 ionice -c2 -n7 ./script/clippy \
  && nice -n 19 ionice -c2 -n7 cargo test -p ggo_emu_wasm -p ggo_emu_panel -p ggo_audio_panel -p ggo_smoke -p ggo_world_panel
```
- [ ] **Step 3: Manual check (controller, not subagent):** launch zed, open a cart in the emu pane, confirm it runs with audio; point `~/.ggo/emulator.json` at `/home/clay/projects/ggo/tools/target/wasm32-unknown-unknown/release/ggo_emu.wasm`, rebuild it in ggo, confirm the running session restarts and the status row shows `Local: …`.
- [ ] **Step 4:** Commit any fixes from Steps 1-2 with the same verify-then-commit chain.
