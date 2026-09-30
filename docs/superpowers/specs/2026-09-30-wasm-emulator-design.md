# WASM emulator runtime — design

Date: 2026-09-30
Status: approved (brainstorm), pending spec review

## Problem

zedgg links `ggo-emu-core` (and friends) natively as path deps on `../ggo`.
Switching emulator versions means rebuilding zed; nothing can be updated
live. We want the emulator runtime loaded as a WebAssembly module whose
source is pluggable (bundled, local file, HTTP URL, Forgejo release), and
swappable without restarting zed.

## Scope

In scope: the emulator **runtime** — CPU/MMU/peripherals/run loop,
framebuffer, input, UART/comm, APU (including the audio panel's preview
APU), PPU snapshot, perf, save state.

Out of scope (stay native path deps): data-format and tooling crates —
`ggo-worldlib`, `ggo-asset-formats`, `ggo-audio`, `ggo-db`, `ggo-comm`,
`ggo-wire`, `emerald-world`, `emerald-core`, host side of
`emerald-editor-link`. They change rarely and are not the emulator.

Non-goals:
- Hot-swap with machine-state carry-over. Swap restarts sessions.
- Per-project version pinning. Selection is a user setting.
- Building wasm from source inside zed. Sources provide prebuilt `.wasm`.
- Forgejo CI publishing workflow (no instance yet; added later).

## Current state (2026-09-30)

- Runtime use concentrated in `crates/ggo/emu_panel/src/drive.rs`
  (cart parse, sandbox plan, `Mmu`, `Cpu`, `Peripherals`,
  `run::run_until_event`, save files, APU `copy_since`, PPU
  `snapshot_into`, perf JSON, EMWD RAM tap read/write), plus
  `link.rs` (`uart_inject`, `take_comm`) and
  `crates/ggo/audio_panel/src/preview.rs` (standalone `Apu`).
- `hardware.rs` / emu MCP `hw_env` read `ggo_emu_core::BUILT_FROM_COMMIT`
  for version-skew checks.
- Each session runs on a dedicated OS thread (`PerfSim` is `!Send`),
  paced at 16.667 ms, frames to gpui over `async_channel::bounded(1)`.
  320×240 RGB565 → BGRA; audio 32020 Hz mono i16.
- ggo already has a browser C-ABI cdylib: `tools/ggo-emu/src/wasm.rs`
  (`ggo_emu_new`, `run_frame`, `framebuffer`, `take_uart`, `read_ram`,
  `perf_json`, audio ring, save ptr/len/dirty, …).
- zed already depends on wasmtime 36 (`extension_host`).

## Design

### 1. ggo side — extend the C ABI

One artifact, `ggo_emu.wasm` (wasm32-unknown-unknown), built from
`tools/ggo-emu`, serves both the browser and zed.

New exports in `tools/ggo-emu/src/wasm.rs`:

| Export | Purpose |
|---|---|
| `ggo_abi_version() -> u32` | `major << 16 \| minor`. Host requires equal major, minor ≥ its minimum. |
| `ggo_emu_build_commit()` ptr + len | Replaces native `BUILT_FROM_COMMIT`. |
| `ggo_emu_ppu_snapshot(emu)` ptr + len | Debug views (`PpuSnapshot` bytes). |
| `ggo_emu_write_ram(emu, addr, ptr, len) -> i32` | EMWD tap enable word. |
| `ggo_emu_uart_inject(emu, ptr, len)` | `link.rs` host→cart bytes. |
| `ggo_emu_take_comm(emu)` ptr + len | `link.rs` cart→host comm bytes. |
| `ggo_emu_set_input(emu, mask)`, `ggo_emu_set_ticks_ms(emu, ms)` | Only if `run_frame` does not already take them. |
| `ggo_emu_run_frame` return code + `ggo_emu_fault_detail(emu)` ptr + len | Encodes `FrameEvent::{Vsync, Budget, Exit, Fault}`, `Trap::PmpFault`, arena overrun. |
| `ggo_emu_arena_end(emu) -> u32` | EMWD tap scan bound (`mmu.plan.arena_end()`). |
| `ggo_apu_*` (new/free/queue_samples/play_sample/run_frame/copy_since) | Audio panel preview. |

Filesystem stays out of the module: no WASI, no host fs imports. The
host reads the save file and card-dir asset files and passes the bytes
into `ggo_emu_new` (extend its signature or add `ggo_emu_add_asset`
before first frame); the host polls `save_dirty`, reads `save_ptr/len`,
writes the file, and clears the flag. The module is a pure function of
its inputs, which keeps network-fetched modules sandboxed.

PPU/screen constants and `rgb565_to_argb` used by zed debug UI move to
a native no_std crate that changes rarely (`ggo-hal` or `ggo-wire`), so
zed drops its `ggo-emu-core` dependency entirely.

### 2. zed side — sources and runtime

New crate `crates/ggo/emu_wasm` (`[lib] path = "src/ggo_emu_wasm.rs"`).

```rust
pub trait EmulatorSource: Send + Sync {
    fn describe(&self) -> SharedString;
    fn list_versions(&self, cx: &App) -> Task<Result<Vec<EmulatorVersion>>>;
    fn fetch(&self, version: &EmulatorVersion, cx: &App) -> Task<Result<Arc<[u8]>>>;
}
```

Implementations:

- `BundledSource` — `include_bytes!` of checked-in
  `assets/ggo/ggo_emu.wasm`. `script/update-bundled-ggo-emu` builds it
  from `../ggo` and copies it in. Always-available fallback.
- `LocalSource { path }` — reads the file; watches it via `Fs::watch`;
  a change triggers re-fetch → swap. Dev loop.
- `HttpSource { url }` — single version; ETag (or content hash) as key.
- `ForgejoSource { base_url, owner, repo, asset_name, token: Option }` —
  `GET {base_url}/api/v1/repos/{owner}/{repo}/releases` (Gitea-compatible);
  versions = release tags; `tag: "latest"` resolves to newest
  non-draft, non-prerelease; downloads the asset named `asset_name`
  (`browser_download_url`).

Setting `ggo.emulator.source` (default `"bundled"`):

```json
"ggo": { "emulator": { "source": "bundled" } }
"ggo": { "emulator": { "source": { "path": "../ggo/target/wasm32-unknown-unknown/release/ggo_emu.wasm" } } }
"ggo": { "emulator": { "source": { "url": "https://…/ggo_emu.wasm" } } }
"ggo": { "emulator": { "source": { "forgejo": { "base_url": "https://git.example", "owner": "gemdropgames", "repo": "ggo", "asset": "ggo_emu.wasm", "tag": "latest" } } } }
```

Cache: `paths::data_dir()/ggo-emu/<sha256>.wasm` plus
`<sha256>-wasmtime<ver>.cwasm` (`Module::serialize`) so repeat loads skip
Cranelift and network sources work offline once cached.

`EmuRuntime` (gpui `Global`):
- Holds `Option<Arc<LoadedEmulator>>` (engine, module, ABI version,
  build commit, source label).
- Observes the setting → resolve source → fetch → sha256 → cache →
  compile/deserialize → validate ABI → publish → emit
  `EmulatorChanged`.
- On failure: keep previous module, show error toast. If nothing is
  loaded yet, load bundled.

`WasmEmu` — one instance (`Store` + `Instance`), safe methods mirroring
what `drive.rs` needs. Copies bytes out of linear memory; never hands a
borrow across the boundary.

Picker: action `ggo: select emulator version`, built on the shared
picker card (d145aca56a). Lists Bundled, Local (current path), Forgejo
tags, configured URL; selecting writes the setting.

### 3. Integration

- `drive.rs` keeps its dedicated thread; the thread owns the
  `WasmEmu` store. `LoadedEmulator` is `Send + Sync` and shared.
- Direct `ggo-emu-core` calls in `drive.rs`, `link.rs`, `preview.rs`
  become `WasmEmu` / wasm-APU calls. Frame channel, control atomics,
  BGRA conversion unchanged.
- Emu panel and `viewer_run` subscribe to `EmulatorChanged`: for each
  running session flush save → stop → restart from cart boot on the
  new module.
- `hardware.rs` skew check and MCP `emu_status` / `hw_env` report the
  active module's source label + build commit.

### Errors

- Fetch / ABI mismatch / compile errors → toast; previous module stays.
- Wasm trap during a frame → `FrameEvent::Fault` with trap message; that
  session ends, zed survives.
- Wasmtime epoch interruption: epoch deadline per frame as outer guard
  against a hung module (native 5M-instruction budget still applies
  inside).

## Testing

1. **Perf gate (first plan task, spike):** run a real cart 600 frames
   native vs wasmtime. Gate: wasm sustains 60 fps at 1× with headroom.
   Failure stops the project for a rethink.
2. Unit: each source against a mock HTTP server (Forgejo release JSON
   fixtures, `latest` resolution, missing asset); cache hit/miss; ABI
   accept/reject.
3. Integration: boot a fixture cart through `WasmEmu`; framebuffer CRC
   and UART output match goldens recorded once from the native run over
   N frames (goldens checked in, so no native dev-dep remains).
4. Swap: `LocalSource` file rewrite → `EmulatorChanged` fires →
   session restarts.

## Rollout

ggo branch (ABI additions) and zed branch `wasm-emulator` land together.
Dropping the `ggo-emu-core` dependency from zed is the final task.
