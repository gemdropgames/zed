//! Hosts the GGO emulator wasm module (`ggo_emu.wasm`) with wasmtime.
//!
//! The module may be the bundled one or one fetched from the network, so the
//! host treats it as untrusted: every guest pointer is bounds-checked, asset
//! reads are confined to the cart's own directory, and every call runs under
//! an epoch deadline so a wedged module traps instead of freezing the caller.

use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use ggo_emu_abi::{
    NEW_FLAG_HOST_ASSETS, NEW_FLAG_LOG_SINK, PpuSnapshot, SCREEN_PIXELS, STATUS_BUDGET,
    STATUS_EXITED, STATUS_FAULTED, STATUS_OOM, STATUS_VSYNC, abi_major, abi_minor,
};
use gpui::SharedString;
use util::ResultExt as _;
use wasmtime::{Caller, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

#[cfg(any(test, feature = "test-support"))]
pub mod fixture;
pub mod runtime;
pub mod sources;

pub use runtime::*;

pub const REQUIRED_ABI_MAJOR: u16 = 1;
pub const MIN_ABI_MINOR: u16 = 0;
pub static BUNDLED_WASM: &[u8] = include_bytes!("../bundled/ggo_emu.wasm");

/// Epoch ticks per guest call before a wedged module is trapped. The ticker
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
            .map_err(|error| format!("starting the epoch ticker: {error}"))?;
        Ok(engine)
    });
    engine
        .as_ref()
        .map_err(|error| anyhow!("wasmtime engine: {error}"))
}

/// `start..start + len` for a guest-supplied pointer and length, or `None`
/// when the sum overflows.
fn byte_range(start: usize, len: usize) -> Option<std::ops::Range<usize>> {
    Some(start..start.checked_add(len)?)
}

/// The arena's bytes inside linear memory; `None` if the guest-reported
/// bounds overflow or are inverted.
fn arena_range(psram: usize, info: &CartInfo) -> Option<std::ops::Range<usize>> {
    let start = psram.checked_add(info.arena_start)?;
    let end = psram.checked_add(info.arena_end)?;
    (start <= end).then_some(start..end)
}

struct HostState {
    card_dir: Option<PathBuf>,
}

/// The guest's asset lookups resolve only inside the cart's own directory,
/// because the module may come from the network.
fn read_card_file(card_dir: &Path, path: &str) -> Option<Vec<u8>> {
    let relative = Path::new(path);
    if relative.is_absolute()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }
    std::fs::read(card_dir.join(relative)).ok()
}

fn linker() -> Result<Linker<HostState>> {
    let mut linker = Linker::new(engine()?);
    linker.func_wrap(
        "env",
        "ggo_host_read_asset",
        |mut caller: Caller<'_, HostState>,
         path_ptr: u32,
         path_len: u32,
         dst_ptr: u32,
         dst_cap: u32|
         -> i64 {
            let Some(memory) = caller
                .get_export("memory")
                .and_then(|export| export.into_memory())
            else {
                return -1;
            };
            let Some(card_dir) = caller.data().card_dir.clone() else {
                return -1;
            };
            let start = path_ptr as usize;
            let Some(path) = byte_range(start, path_len as usize)
                .and_then(|range| memory.data(&caller).get(range))
                .map(|raw| String::from_utf8_lossy(raw).into_owned())
            else {
                return -1;
            };
            let Some(bytes) = read_card_file(&card_dir, &path) else {
                return -1;
            };
            // A buffer too small for the file is a size query: report the
            // length and write nothing.
            if bytes.len() <= dst_cap as usize
                && memory.write(&mut caller, dst_ptr as usize, &bytes).is_err()
            {
                return -1;
            }
            bytes.len() as i64
        },
    )?;
    Ok(linker)
}

fn instantiate(
    module: &Module,
    card_dir: Option<PathBuf>,
) -> Result<(Store<HostState>, Instance, Memory)> {
    let mut store = Store::new(engine()?, HostState { card_dir });
    store.set_epoch_deadline(TURN_DEADLINE_TICKS);
    let instance = linker()?
        .instantiate(&mut store, module)
        .context("instantiating the emulator module")?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .context("emulator module exports no memory")?;
    Ok((store, instance, memory))
}

/// A compiled emulator module. Compiling once and instantiating per cart
/// keeps cart start-up cheap.
pub struct LoadedEmulator {
    pub label: SharedString,
    pub abi_version: u32,
    pub build_commit: Option<String>,
    module: Module,
}

impl LoadedEmulator {
    #[allow(
        clippy::absurd_extreme_comparisons,
        reason = "MIN_ABI_MINOR is 0 until the ABI grows a minor revision"
    )]
    pub fn compile(bytes: &[u8], label: impl Into<SharedString>) -> Result<LoadedEmulator> {
        let module = Module::new(engine()?, bytes).context("compiling the emulator module")?;

        let mut store = Store::new(engine()?, HostState { card_dir: None });
        store.set_epoch_deadline(TURN_DEADLINE_TICKS);
        let instance = linker()?
            .instantiate(&mut store, &module)
            .context("instantiating the emulator module")?;

        let abi_version = instance
            .get_typed_func::<(), u32>(&mut store, "ggo_abi_version")
            .context("emulator module has no ggo_abi_version export (pre-ABI build?)")?
            .call(&mut store, ())?;
        ensure!(
            abi_major(abi_version) == REQUIRED_ABI_MAJOR && abi_minor(abi_version) >= MIN_ABI_MINOR,
            "emulator module ABI {}.{} is incompatible (need {REQUIRED_ABI_MAJOR}.{MIN_ABI_MINOR}+)",
            abi_major(abi_version),
            abi_minor(abi_version),
        );

        let memory = instance
            .get_memory(&mut store, "memory")
            .context("emulator module exports no memory")?;
        let commit_ptr = instance
            .get_typed_func::<(), u32>(&mut store, "ggo_build_commit_ptr")?
            .call(&mut store, ())? as usize;
        let commit_len = instance
            .get_typed_func::<(), u32>(&mut store, "ggo_build_commit_len")?
            .call(&mut store, ())? as usize;
        let build_commit = memory
            .data(&store)
            .get(byte_range(commit_ptr, commit_len).context("build commit range overflows")?)
            .map(|raw| String::from_utf8_lossy(raw).into_owned())
            .filter(|commit| !commit.is_empty());

        Ok(LoadedEmulator {
            label: label.into(),
            abi_version,
            build_commit,
            module,
        })
    }

    pub fn start_cart(&self, cart: &[u8], seed: u64, card_dir: Option<PathBuf>) -> Result<WasmEmu> {
        let mut guest = Guest::new(&self.module, card_dir)?;
        let funcs = EmuFuncs::resolve(&mut guest)?;

        let cart_ptr = guest.stage(cart)?;
        let created = funcs.new_ex.call(
            &mut guest.store,
            (
                cart_ptr,
                cart.len() as u32,
                seed as u32,
                (seed >> 32) as u32,
                NEW_FLAG_LOG_SINK | NEW_FLAG_HOST_ASSETS,
            ),
        );
        let freed = guest.unstage(cart_ptr, cart.len());
        let handle = created?;
        freed?;
        if handle == 0 {
            bail!("cart failed to parse");
        }

        // From here `emu` owns the handle, so an error below still frees it.
        let mut emu = WasmEmu {
            guest,
            funcs,
            handle,
            info: CartInfo::default(),
        };
        let info_ptr = emu.funcs.info_json.call(&mut emu.guest.store, handle)?;
        let info_len = emu.funcs.info_json_len.call(&mut emu.guest.store, handle)?;
        let raw = emu.guest.read_bytes(info_ptr, info_len as usize)?;
        emu.info = serde_json::from_slice(&raw).context("parsing the cart info")?;
        Ok(emu)
    }

    pub fn new_apu(&self) -> Result<WasmApu> {
        let mut guest = Guest::new(&self.module, None)?;
        let funcs = ApuFuncs::resolve(&mut guest)?;
        let handle = funcs.new.call(&mut guest.store, ())?;
        ensure!(handle != 0, "emulator module could not allocate an APU");
        Ok(WasmApu {
            guest,
            funcs,
            handle,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEvent {
    Vsync(u32),
    Budget,
    Exit(i32),
    Fault(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct CartInfo {
    pub title: String,
    pub save_bytes: usize,
    pub arena_start: usize,
    pub arena_end: usize,
    pub arena_len: usize,
    pub vram_pool: usize,
    pub ram_pool: usize,
    pub body_truncated: bool,
}

/// One module instance: its store, linear memory and the allocator exports.
struct Guest {
    store: Store<HostState>,
    instance: Instance,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    free: TypedFunc<(u32, u32), ()>,
}

impl Guest {
    fn new(module: &Module, card_dir: Option<PathBuf>) -> Result<Guest> {
        let (mut store, instance, memory) = instantiate(module, card_dir)?;
        let alloc = instance.get_typed_func(&mut store, "ggo_alloc")?;
        let free = instance.get_typed_func(&mut store, "ggo_free")?;
        Ok(Guest {
            store,
            instance,
            memory,
            alloc,
            free,
        })
    }

    fn func<Params, Results>(&mut self, name: &str) -> Result<TypedFunc<Params, Results>>
    where
        Params: wasmtime::WasmParams,
        Results: wasmtime::WasmResults,
    {
        self.instance
            .get_typed_func(&mut self.store, name)
            .with_context(|| format!("emulator module export {name} is missing or mismatched"))
    }

    fn arm(&mut self) {
        self.store.set_epoch_deadline(TURN_DEADLINE_TICKS);
    }

    fn stage(&mut self, bytes: &[u8]) -> Result<u32> {
        self.arm();
        let ptr = self.alloc.call(&mut self.store, bytes.len() as u32)?;
        let start = ptr as usize;
        let range = byte_range(start, bytes.len())
            .ok_or_else(|| anyhow!("emulator allocated an out-of-bounds buffer"))?;
        self.memory
            .data_mut(&mut self.store)
            .get_mut(range)
            .ok_or_else(|| anyhow!("emulator allocated an out-of-bounds buffer"))?
            .copy_from_slice(bytes);
        Ok(ptr)
    }

    fn unstage(&mut self, ptr: u32, len: usize) -> Result<()> {
        self.arm();
        self.free.call(&mut self.store, (ptr, len as u32))
    }

    fn read_bytes(&mut self, ptr: u32, len: usize) -> Result<Vec<u8>> {
        let start = ptr as usize;
        byte_range(start, len)
            .and_then(|range| self.memory.data(&self.store).get(range))
            .map(<[u8]>::to_vec)
            .ok_or_else(|| anyhow!("emulator returned an out-of-bounds buffer {start:#x}+{len}"))
    }

    fn read_samples(&mut self, ptr: u32, count: usize) -> Result<Vec<i16>> {
        Ok(self
            .read_bytes(ptr, count.checked_mul(2).context("sample count overflows")?)?
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect())
    }
}

struct EmuFuncs {
    new_ex: TypedFunc<(u32, u32, u32, u32, u32), u32>,
    free: TypedFunc<u32, ()>,
    run_turn: TypedFunc<(u32, u32, u32), u32>,
    frame_number: TypedFunc<u32, u32>,
    exit_code: TypedFunc<u32, i32>,
    fault_detail_ptr: TypedFunc<u32, u32>,
    fault_detail_len: TypedFunc<u32, u32>,
    info_json: TypedFunc<u32, u32>,
    info_json_len: TypedFunc<u32, u32>,
    framebuffer: TypedFunc<u32, u32>,
    take_uart: TypedFunc<u32, u32>,
    uart_len: TypedFunc<u32, u32>,
    uart_inject: TypedFunc<(u32, u32, u32), ()>,
    take_comm: TypedFunc<u32, u32>,
    comm_len: TypedFunc<u32, u32>,
    audio_copy_since: TypedFunc<(u32, u64), u64>,
    audio_staged_ptr: TypedFunc<u32, u32>,
    audio_staged_len: TypedFunc<u32, u32>,
    ppu_snapshot: TypedFunc<u32, u32>,
    ppu_snapshot_len: TypedFunc<u32, u32>,
    psram_ptr: TypedFunc<u32, u32>,
    save_ptr: TypedFunc<u32, u32>,
    save_len: TypedFunc<u32, u32>,
    save_dirty: TypedFunc<u32, u32>,
    clear_save_dirty: TypedFunc<u32, ()>,
    save_write: TypedFunc<(u32, u32, u32), ()>,
    perf_json: TypedFunc<u32, u32>,
    perf_json_len: TypedFunc<u32, u32>,
    perf_frames: TypedFunc<u32, u32>,
}

impl EmuFuncs {
    fn resolve(guest: &mut Guest) -> Result<EmuFuncs> {
        Ok(EmuFuncs {
            new_ex: guest.func("ggo_emu_new_ex")?,
            free: guest.func("ggo_emu_free")?,
            run_turn: guest.func("ggo_emu_run_turn")?,
            frame_number: guest.func("ggo_emu_frame_number")?,
            exit_code: guest.func("ggo_emu_exit_code")?,
            fault_detail_ptr: guest.func("ggo_emu_fault_detail_ptr")?,
            fault_detail_len: guest.func("ggo_emu_fault_detail_len")?,
            info_json: guest.func("ggo_emu_info_json")?,
            info_json_len: guest.func("ggo_emu_info_json_len")?,
            framebuffer: guest.func("ggo_emu_framebuffer")?,
            take_uart: guest.func("ggo_emu_take_uart")?,
            uart_len: guest.func("ggo_emu_uart_len")?,
            uart_inject: guest.func("ggo_emu_uart_inject")?,
            take_comm: guest.func("ggo_emu_take_comm")?,
            comm_len: guest.func("ggo_emu_comm_len")?,
            audio_copy_since: guest.func("ggo_emu_audio_copy_since")?,
            audio_staged_ptr: guest.func("ggo_emu_audio_staged_ptr")?,
            audio_staged_len: guest.func("ggo_emu_audio_staged_len")?,
            ppu_snapshot: guest.func("ggo_emu_ppu_snapshot")?,
            ppu_snapshot_len: guest.func("ggo_emu_ppu_snapshot_len")?,
            psram_ptr: guest.func("ggo_emu_psram_ptr")?,
            save_ptr: guest.func("ggo_emu_save_ptr")?,
            save_len: guest.func("ggo_emu_save_len")?,
            save_dirty: guest.func("ggo_emu_save_dirty")?,
            clear_save_dirty: guest.func("ggo_emu_clear_save_dirty")?,
            save_write: guest.func("ggo_emu_save_write")?,
            perf_json: guest.func("ggo_emu_perf_json")?,
            perf_json_len: guest.func("ggo_emu_perf_json_len")?,
            perf_frames: guest.func("ggo_emu_perf_frames")?,
        })
    }
}

/// A running cart. Methods take `&mut self` because every guest call needs
/// the store; none hold a memory slice across a guest call, since the guest
/// may grow (and so move) its linear memory.
pub struct WasmEmu {
    guest: Guest,
    funcs: EmuFuncs,
    handle: u32,
    info: CartInfo,
}

impl WasmEmu {
    pub fn info(&self) -> &CartInfo {
        &self.info
    }

    pub fn run_turn(&mut self, input: u32, ticks_ms: u32) -> TurnEvent {
        match self.try_run_turn(input, ticks_ms) {
            Ok(event) => event,
            Err(error) => TurnEvent::Fault(format!("emulator trapped: {error}")),
        }
    }

    fn try_run_turn(&mut self, input: u32, ticks_ms: u32) -> Result<TurnEvent> {
        self.guest.arm();
        let handle = self.handle;
        let status = self
            .funcs
            .run_turn
            .call(&mut self.guest.store, (handle, input, ticks_ms))?;
        Ok(match status {
            STATUS_VSYNC => TurnEvent::Vsync(
                self.funcs
                    .frame_number
                    .call(&mut self.guest.store, handle)?,
            ),
            STATUS_BUDGET => TurnEvent::Budget,
            STATUS_EXITED => {
                TurnEvent::Exit(self.funcs.exit_code.call(&mut self.guest.store, handle)?)
            }
            STATUS_FAULTED | STATUS_OOM => {
                let ptr = self
                    .funcs
                    .fault_detail_ptr
                    .call(&mut self.guest.store, handle)?;
                let len = self
                    .funcs
                    .fault_detail_len
                    .call(&mut self.guest.store, handle)?;
                let detail = self.guest.read_bytes(ptr, len as usize)?;
                TurnEvent::Fault(String::from_utf8_lossy(&detail).into_owned())
            }
            unknown => TurnEvent::Fault(format!("emulator returned unknown status {unknown}")),
        })
    }

    pub fn framebuffer_bgra(&mut self) -> Result<Vec<u8>> {
        self.guest.arm();
        let ptr = self
            .funcs
            .framebuffer
            .call(&mut self.guest.store, self.handle)?;
        let mut pixels = self.guest.read_bytes(ptr, SCREEN_PIXELS * 4)?;
        pixels
            .chunks_exact_mut(4)
            .for_each(|pixel| pixel.swap(0, 2));
        Ok(pixels)
    }

    pub fn take_log(&mut self) -> Result<Vec<u8>> {
        self.guest.arm();
        let ptr = self
            .funcs
            .take_uart
            .call(&mut self.guest.store, self.handle)?;
        let len = self
            .funcs
            .uart_len
            .call(&mut self.guest.store, self.handle)?;
        self.guest.read_bytes(ptr, len as usize)
    }

    pub fn uart_inject(&mut self, bytes: &[u8]) -> Result<()> {
        let ptr = self.guest.stage(bytes)?;
        self.guest.arm();
        let called = self.funcs.uart_inject.call(
            &mut self.guest.store,
            (self.handle, ptr, bytes.len() as u32),
        );
        let freed = self.guest.unstage(ptr, bytes.len());
        called?;
        freed
    }

    pub fn take_comm(&mut self) -> Result<Vec<u8>> {
        self.guest.arm();
        let ptr = self
            .funcs
            .take_comm
            .call(&mut self.guest.store, self.handle)?;
        let len = self
            .funcs
            .comm_len
            .call(&mut self.guest.store, self.handle)?;
        self.guest.read_bytes(ptr, len as usize)
    }

    /// Appends to `out` like `Apu::copy_since` and returns the new cursor.
    pub fn audio_copy_since(&mut self, cursor: u64, out: &mut Vec<i16>) -> Result<u64> {
        self.guest.arm();
        let next = self
            .funcs
            .audio_copy_since
            .call(&mut self.guest.store, (self.handle, cursor))?;
        let ptr = self
            .funcs
            .audio_staged_ptr
            .call(&mut self.guest.store, self.handle)?;
        let count = self
            .funcs
            .audio_staged_len
            .call(&mut self.guest.store, self.handle)?;
        out.extend(self.guest.read_samples(ptr, count as usize)?);
        Ok(next)
    }

    pub fn ppu_snapshot(&mut self) -> Result<PpuSnapshot> {
        self.guest.arm();
        let ptr = self
            .funcs
            .ppu_snapshot
            .call(&mut self.guest.store, self.handle)?;
        let len = self
            .funcs
            .ppu_snapshot_len
            .call(&mut self.guest.store, self.handle)?;
        let raw = self.guest.read_bytes(ptr, len as usize)?;
        PpuSnapshot::decode(&raw).context("emulator returned a malformed PPU snapshot")
    }

    /// Runs `f` over the cart's RAM arena, which lives inside the guest's
    /// PSRAM die.
    pub fn with_arena<R>(&mut self, f: impl FnOnce(&mut [u8]) -> R) -> Result<R> {
        self.guest.arm();
        let psram = self
            .funcs
            .psram_ptr
            .call(&mut self.guest.store, self.handle)? as usize;
        let range = arena_range(psram, &self.info).context("arena range is invalid")?;
        let data = self.guest.memory.data_mut(&mut self.guest.store);
        let arena = data.get_mut(range).context("arena outside linear memory")?;
        Ok(f(arena))
    }

    pub fn save_dirty(&mut self) -> Result<bool> {
        self.guest.arm();
        Ok(self
            .funcs
            .save_dirty
            .call(&mut self.guest.store, self.handle)?
            != 0)
    }

    pub fn save_bytes(&mut self) -> Result<Vec<u8>> {
        self.guest.arm();
        let len = self
            .funcs
            .save_len
            .call(&mut self.guest.store, self.handle)?;
        if len == 0 {
            return Ok(Vec::new());
        }
        let ptr = self
            .funcs
            .save_ptr
            .call(&mut self.guest.store, self.handle)?;
        self.guest.read_bytes(ptr, len as usize)
    }

    pub fn write_save(&mut self, bytes: &[u8]) -> Result<()> {
        let ptr = self.guest.stage(bytes)?;
        self.guest.arm();
        let called = self.funcs.save_write.call(
            &mut self.guest.store,
            (self.handle, ptr, bytes.len() as u32),
        );
        let freed = self.guest.unstage(ptr, bytes.len());
        called?;
        freed
    }

    pub fn clear_save_dirty(&mut self) -> Result<()> {
        self.guest.arm();
        self.funcs
            .clear_save_dirty
            .call(&mut self.guest.store, self.handle)
    }

    pub fn perf_json(&mut self) -> Result<String> {
        self.guest.arm();
        let ptr = self
            .funcs
            .perf_json
            .call(&mut self.guest.store, self.handle)?;
        let len = self
            .funcs
            .perf_json_len
            .call(&mut self.guest.store, self.handle)?;
        String::from_utf8(self.guest.read_bytes(ptr, len as usize)?)
            .context("emulator returned non-UTF-8 perf JSON")
    }

    pub fn perf_frames(&mut self) -> Result<u64> {
        self.guest.arm();
        Ok(u64::from(
            self.funcs
                .perf_frames
                .call(&mut self.guest.store, self.handle)?,
        ))
    }
}

impl Drop for WasmEmu {
    fn drop(&mut self) {
        self.guest.arm();
        self.funcs
            .free
            .call(&mut self.guest.store, self.handle)
            .log_err();
    }
}

struct ApuFuncs {
    new: TypedFunc<(), u32>,
    free: TypedFunc<u32, ()>,
    queue_samples: TypedFunc<(u32, u32, u32, u32), i32>,
    play_sample: TypedFunc<(u32, u32, u32, u32, u32, u32, u32), i32>,
    run_frame: TypedFunc<u32, ()>,
    copy_since: TypedFunc<(u32, u64), u64>,
    staged_ptr: TypedFunc<u32, u32>,
    staged_len: TypedFunc<u32, u32>,
}

impl ApuFuncs {
    fn resolve(guest: &mut Guest) -> Result<ApuFuncs> {
        Ok(ApuFuncs {
            new: guest.func("ggo_apu_new")?,
            free: guest.func("ggo_apu_free")?,
            queue_samples: guest.func("ggo_apu_queue_samples")?,
            play_sample: guest.func("ggo_apu_play_sample")?,
            run_frame: guest.func("ggo_apu_run_frame")?,
            copy_since: guest.func("ggo_apu_copy_since")?,
            staged_ptr: guest.func("ggo_apu_staged_ptr")?,
            staged_len: guest.func("ggo_apu_staged_len")?,
        })
    }
}

/// A standalone APU, for previewing samples without a running cart.
pub struct WasmApu {
    guest: Guest,
    funcs: ApuFuncs,
    handle: u32,
}

impl WasmApu {
    pub fn queue_samples(&mut self, vram_off: u32, bytes: &[u8]) -> Result<i32> {
        let ptr = self.guest.stage(bytes)?;
        self.guest.arm();
        let called = self.funcs.queue_samples.call(
            &mut self.guest.store,
            (self.handle, vram_off, ptr, bytes.len() as u32),
        );
        let freed = self.guest.unstage(ptr, bytes.len());
        let copied = called?;
        freed?;
        Ok(copied)
    }

    pub fn play_sample(
        &mut self,
        ch: u32,
        start_off: u32,
        end_off: u32,
        loop_off: u32,
        step_vol: u32,
        adsr: u32,
    ) -> Result<i32> {
        self.guest.arm();
        self.funcs.play_sample.call(
            &mut self.guest.store,
            (
                self.handle,
                ch,
                start_off,
                end_off,
                loop_off,
                step_vol,
                adsr,
            ),
        )
    }

    pub fn run_frame(&mut self) -> Result<()> {
        self.guest.arm();
        self.funcs
            .run_frame
            .call(&mut self.guest.store, self.handle)
    }

    /// Appends to `out` like `Apu::copy_since` and returns the new cursor.
    pub fn copy_since(&mut self, cursor: u64, out: &mut Vec<i16>) -> Result<u64> {
        self.guest.arm();
        let next = self
            .funcs
            .copy_since
            .call(&mut self.guest.store, (self.handle, cursor))?;
        let ptr = self
            .funcs
            .staged_ptr
            .call(&mut self.guest.store, self.handle)?;
        let count = self
            .funcs
            .staged_len
            .call(&mut self.guest.store, self.handle)?;
        out.extend(self.guest.read_samples(ptr, count as usize)?);
        Ok(next)
    }
}

impl Drop for WasmApu {
    fn drop(&mut self) {
        self.guest.arm();
        self.funcs
            .free
            .call(&mut self.guest.store, self.handle)
            .log_err();
    }
}

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
                event @ TurnEvent::Vsync(_) => {
                    vsyncs.push(event);
                    if vsyncs.len() == count {
                        break;
                    }
                }
                TurnEvent::Budget => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        vsyncs
    }

    /// A hand-written module exporting only `ggo_abi_version() -> version`.
    fn wat_free_abi_module(version: u32) -> Vec<u8> {
        let mut sleb = Vec::new();
        let mut value = version as i32;
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0) {
                sleb.push(byte);
                break;
            }
            sleb.push(byte | 0x80);
        }
        let name = b"ggo_abi_version";
        let mut body = vec![0x00, 0x41];
        body.extend(&sleb);
        body.push(0x0B);

        let mut module = b"\0asm\x01\0\0\0".to_vec();
        module.extend([0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F]);
        module.extend([0x03, 0x02, 0x01, 0x00]);
        module.extend([0x07, (name.len() + 4) as u8, 0x01, name.len() as u8]);
        module.extend(name);
        module.extend([0x00, 0x00]);
        module.extend([0x0A, (body.len() + 2) as u8, 0x01, body.len() as u8]);
        module.extend(body);
        module
    }

    #[test]
    fn bundled_module_reports_a_compatible_abi() {
        let emulator = loaded();
        assert_eq!(
            ggo_emu_abi::abi_major(emulator.abi_version),
            REQUIRED_ABI_MAJOR
        );
    }

    #[test]
    fn a_module_with_the_wrong_abi_major_is_rejected() {
        let bytes = wat_free_abi_module(2 << 16);
        let error = LoadedEmulator::compile(&bytes, "bad")
            .err()
            .expect("rejected");
        assert!(error.to_string().contains("ABI"), "{error}");
    }

    #[test]
    fn the_green_cart_presents_green_frames() {
        let mut emu = loaded()
            .start_cart(&fixture::green_screen_cart(), 1, None)
            .expect("starts");
        assert_eq!(run_until_vsyncs(&mut emu, 3).len(), 3);
        let bgra = emu.framebuffer_bgra().expect("framebuffer");
        assert_eq!(bgra.len(), ggo_emu_abi::SCREEN_PIXELS * 4);
        assert_eq!(&bgra[..4], &[0x00, 0xFF, 0x00, 0xFF]);
        assert_eq!(
            emu.ppu_snapshot().expect("snapshot").tiles.len(),
            ggo_emu_abi::VRAM_TILE_CAP * ggo_emu_abi::TILE_BYTES
        );
    }

    #[test]
    fn the_overrun_cart_faults_out_of_memory() {
        let mut emu = loaded()
            .start_cart(&fixture::overrun_cart(), 1, None)
            .expect("starts");
        let event = (0..100)
            .map(|turn| emu.run_turn(0, turn))
            .find(|event| !matches!(event, TurnEvent::Budget | TurnEvent::Vsync(_)));
        assert!(
            matches!(&event, Some(TurnEvent::Fault(reason)) if reason.starts_with("out of memory")),
            "{event:?}"
        );
    }

    #[test]
    fn the_logging_cart_log_reaches_take_log() {
        let mut emu = loaded()
            .start_cart(&fixture::logging_cart(), 1, None)
            .expect("starts");
        run_until_vsyncs(&mut emu, 1);
        assert!(!emu.take_log().expect("log").is_empty());
    }

    #[test]
    fn a_hostile_arena_range_is_an_error_not_a_panic() {
        let info = |arena_start, arena_end| CartInfo {
            arena_start,
            arena_end,
            ..CartInfo::default()
        };
        assert_eq!(arena_range(16, &info(4, 8)), Some(20..24));
        assert_eq!(arena_range(16, &info(8, 4)), None);
        assert_eq!(arena_range(usize::MAX, &info(0, 1)), None);
        assert_eq!(arena_range(16, &info(0, usize::MAX)), None);
        assert_eq!(byte_range(usize::MAX, 1), None);

        let mut emu = loaded()
            .start_cart(&fixture::green_screen_cart(), 1, None)
            .expect("starts");
        emu.info.arena_end = emu.info.arena_start.wrapping_sub(1);
        assert!(emu.with_arena(|_| ()).is_err());
        emu.info.arena_end = usize::MAX;
        assert!(emu.with_arena(|_| ()).is_err());
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
        assert!(
            apu.play_sample(
                0,
                0,
                64,
                ggo_emu_abi::ONE_SHOT,
                0x1000 | (0xFF << 16) | (0xFF << 24),
                0
            )
            .expect("play")
                >= 0
        );
        apu.run_frame().expect("frame");
        let mut out = Vec::new();
        assert!(apu.copy_since(0, &mut out).expect("copy") > 0);
        assert!(!out.is_empty());
    }
}
