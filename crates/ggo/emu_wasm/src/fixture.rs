//! Hand-assembled cart fixtures, shared by the panel's tests and -- through
//! its `test-support` re-export -- `ggo_smoke`'s emulator journeys.

// There is no committed `.cart` anywhere in the ggo repo (checked:
// `find . -name '*.cart' -not -path '*/target/*'` is empty, and
// `tools/ggo-fixture` is a perf-database fixture, not a cart), and
// packing one in test setup would mean a riscv32 toolchain plus
// `emd`. So the drive test assembles its own ten-instruction cart
// instead -- real `GGOC` header, real RV32I machine code, driven
// through the real `drive::start` thread. It is a genuine end-to-end
// check of the port (header parse -> XIP map -> interpret -> ecall
// dispatch -> PPU compose -> RGB565 -> BGRA -> channel), just of a
// cart small enough to write by hand.

use ggo_emu_core::cart::{FLAG_HAS_ASSET_TOC, HEADER_LEN, MAGIC, SUPPORTED_HEADER_VERSION};
use ggo_emu_core::crc32::crc32;

/// The cart header title `green_screen_cart` stamps -- also the
/// perf-JSON `cart` identity, and therefore the `cart.name` row an
/// ingested run of it lands on.
pub const GREEN_CART_TITLE: &str = "Green Fix";

/// `addi rd, x0, imm` -- the load-immediate special case (`rs1 = x0`),
/// the only form [`green_screen_cart`] needs besides `ecall` and a
/// backwards `jal`.
fn addi(rd: u32, imm: i32) -> u32 {
    ((imm as u32 & 0xFFF) << 20) | (rd << 7) | 0x13
}

/// `addi rd, rs1, imm` -- the general two-register form. [`addi`]
/// above is the `rs1 = x0` special case; [`logging_cart`] needs this
/// one to add an offset onto the base [`lui`] just loaded.
fn addi_reg(rd: u32, rs1: u32, imm: i32) -> u32 {
    ((imm as u32 & 0xFFF) << 20) | (rs1 << 15) | (rd << 7) | 0x13
}

/// `lui rd, imm20` -- loads `imm20 << 12` into `rd`. Only
/// [`logging_cart`] needs this, to build the XIP address of its
/// `log()` message; every other fixture instruction fits a single
/// 12-bit `addi` immediate.
fn lui(rd: u32, imm20: u32) -> u32 {
    ((imm20 & 0xF_FFFF) << 12) | (rd << 7) | 0x37
}

/// `sw rs2, imm(rs1)` -- the only store any fixture needs, and only
/// [`overrun_cart`] needs it.
fn sw(rs1: u32, rs2: u32, imm: i32) -> u32 {
    let imm = imm as u32;
    (((imm >> 5) & 0x7F) << 25) | (rs2 << 20) | (rs1 << 15) | (2 << 12) | ((imm & 0x1F) << 7) | 0x23
}

/// `bge rs1, rs2, offset` (offset in bytes, relative to this
/// instruction). The only branch any fixture needs, and only
/// [`comm_echo_cart`] needs it: `bge x0, a0` is "skip unless
/// `comm_recv` returned something".
fn bge(rs1: u32, rs2: u32, offset: i32) -> u32 {
    let imm = offset as u32;
    (((imm >> 12) & 1) << 31)
        | (((imm >> 5) & 0x3F) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (5 << 12)
        | (((imm >> 1) & 0xF) << 8)
        | (((imm >> 11) & 1) << 7)
        | 0x63
}

/// `jal x0, offset` (offset in bytes, relative to this instruction).
fn jal_x0(offset: i32) -> u32 {
    let imm = offset as u32;
    ((imm >> 20) & 1) << 31
        | ((imm >> 1) & 0x3FF) << 21
        | ((imm >> 11) & 1) << 20
        | ((imm >> 12) & 0xFF) << 12
        | 0x6F
}

const ECALL: u32 = 0x0000_0073;
const A0: u32 = 10;
const A1: u32 = 11;
const A2: u32 = 12;
const A7: u32 = 17;

/// Syscall numbers, from `gemdrop_sdk::sys` / `ggo_emu_core::abi`.
const SYS_PRESENT: i32 = 0x00;
const SYS_VSYNC_WAIT: i32 = 0x01;
/// The backdrop is a PPU register (ppu-contract 11.2), not palette entry 0.
const SYS_SET_BACKDROP: i32 = 0x4E;
const SYS_LOG: i32 = 0x4B;
const SYS_SAVE_WRITE: i32 = 0x31;
// Through `ggo_emu_core::abi`, which pins to `gemdrop_sdk::sys`
// itself: this module is compiled into the LIB, where the SDK (a
// dev-dependency, for the link tests) is not in scope.
const SYS_COMM_SEND: i32 = ggo_emu_core::abi::Syscall::CommSend as i32;
const SYS_COMM_RECV: i32 = ggo_emu_core::abi::Syscall::CommRecv as i32;

/// RGB565 green -- 0x07E0, which happens to fit in a 12-bit signed
/// immediate, so the program needs no `lui`.
const GREEN: u16 = 0x07E0;

/// A cart that paints the whole screen green and then presents
/// forever:
///
/// ```text
///     set_backdrop(0x07E0)
/// loop:
///     present()                              ; PPU -> default_fb
///     vsync_wait()                           ; frame boundary
///     j loop
/// ```
///
/// No tile layer is enabled and no sprite is shown, so every pixel
/// falls through to the backdrop -- `ppu.rs`'s
/// `compose_sprite_over_backdrop_with_transparency` documents the
/// same "no tile layer enabled -> backdrop register" path.
pub fn green_screen_cart() -> Vec<u8> {
    let body: Vec<u32> = vec![
        addi(A0, GREEN as i32),     // backdrop colour
        addi(A1, 0),                //
        addi(A2, 0),                //
        addi(A7, SYS_SET_BACKDROP), //
        ECALL,                      //
        addi(A7, SYS_PRESENT),      // loop:
        ECALL,                      //
        addi(A7, SYS_VSYNC_WAIT),   //
        ECALL,                      //
        jal_x0(-16),                // back to `loop`
    ];
    let body: Vec<u8> = body.iter().flat_map(|w| w.to_le_bytes()).collect();

    // Header layout copied from `ggo_emu_core::cart`'s own
    // `make_cart_flags` test helper.
    let mut h = [0u8; HEADER_LEN];
    h[0x00..0x04].copy_from_slice(&MAGIC);
    h[0x04..0x06].copy_from_slice(&SUPPORTED_HEADER_VERSION.to_le_bytes());
    h[0x06..0x08].copy_from_slice(&0u16.to_le_bytes()); // required_abi
    h[0x08..0x08 + GREEN_CART_TITLE.len()].copy_from_slice(GREEN_CART_TITLE.as_bytes());
    h[0x28..0x2C].copy_from_slice(&0u32.to_le_bytes()); // entry_offset
    h[0x2C..0x30].copy_from_slice(&(body.len() as u32).to_le_bytes());
    h[0x30..0x34].copy_from_slice(&0u32.to_le_bytes()); // save_bytes
    h[0x34..0x38].copy_from_slice(&0u32.to_le_bytes()); // ram_needed
    h[0x38..0x3C].copy_from_slice(&0u32.to_le_bytes()); // flags
    let crc = crc32(&h[0x00..0x3C]);
    h[0x3C..0x40].copy_from_slice(&crc.to_le_bytes());

    let mut out = h.to_vec();
    out.extend_from_slice(&body);
    out
}

/// The cart header title [`logging_cart`] stamps.
pub const SAVING_CART_TITLE: &str = "Save Fix";
/// The saving cart's declared save region.
pub const SAVING_CART_SAVE_BYTES: u32 = 64;
/// How many bytes of its own code the saving cart writes at offset 0.
pub const SAVING_CART_WRITE_LEN: usize = 8;

/// A cart that writes the first 8 bytes of its own code into the save
/// region at offset 0 (`save_write(0, XIP_BASE, 8)`), then presents
/// green forever -- so a flushed `.sav` carries a payload the test can
/// predict.
pub fn saving_cart() -> Vec<u8> {
    let xip_base_hi20 = ggo_emu_core::sandbox::XIP_BASE >> 12;
    let body: Vec<u32> = vec![
        addi(A0, 0),                            // off 0
        lui(A1, xip_base_hi20),                 // buf = XIP_BASE
        addi(A2, SAVING_CART_WRITE_LEN as i32), // len
        addi(A7, SYS_SAVE_WRITE),               //
        ECALL,                                  // save_write
        addi(A0, GREEN as i32),                 // backdrop colour
        addi(A1, 0),                            //
        addi(A2, 0),                            //
        addi(A7, SYS_SET_BACKDROP),             //
        ECALL,                                  //
        addi(A7, SYS_PRESENT),                  // loop:
        ECALL,                                  //
        addi(A7, SYS_VSYNC_WAIT),               //
        ECALL,                                  //
        jal_x0(-16),                            // back to `loop`
    ];
    let body: Vec<u8> = body.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut h = [0u8; HEADER_LEN];
    h[0x00..0x04].copy_from_slice(&MAGIC);
    h[0x04..0x06].copy_from_slice(&SUPPORTED_HEADER_VERSION.to_le_bytes());
    h[0x06..0x08].copy_from_slice(&0u16.to_le_bytes()); // required_abi
    h[0x08..0x08 + SAVING_CART_TITLE.len()].copy_from_slice(SAVING_CART_TITLE.as_bytes());
    h[0x28..0x2C].copy_from_slice(&0u32.to_le_bytes()); // entry_offset
    h[0x2C..0x30].copy_from_slice(&(body.len() as u32).to_le_bytes());
    h[0x30..0x34].copy_from_slice(&SAVING_CART_SAVE_BYTES.to_le_bytes());
    h[0x34..0x38].copy_from_slice(&0u32.to_le_bytes()); // ram_needed
    h[0x38..0x3C].copy_from_slice(&0u32.to_le_bytes()); // flags
    let crc = crc32(&h[0x00..0x3C]);
    h[0x3C..0x40].copy_from_slice(&crc.to_le_bytes());
    let mut out = h.to_vec();
    out.extend_from_slice(&body);
    out
}

pub const LOGGING_CART_TITLE: &str = "Log Fix";

/// The message [`logging_cart`]'s single `log()` call emits -- what
/// the end-to-end drive test asserts lands in the console verbatim.
pub const LOG_MESSAGE: &str = "hi from cart";

/// [`green_screen_cart`] plus one real `log(ptr, len)` ecall before
/// the paint loop:
///
/// ```text
///     a0 = XIP_BASE + STR_OFFSET        ; lui + addi -- STR_OFFSET
///                                       ; points at LOG_MESSAGE's
///                                       ; bytes, appended as data
///                                       ; after this program's own
///                                       ; instructions (never
///                                       ; executed, just addressed)
///     a1 = len(LOG_MESSAGE)
///     log()
///     set_backdrop(0x07E0)
/// loop:
///     present()
///     vsync_wait()
///     j loop
/// ```
///
/// Exists to prove the sink `drive::run` attaches carries a REAL
/// guest `log` ecall's bytes end to end -- ggo-ide's equivalent test
/// (`emu/mod.rs::drain_uart_returns_cart_log_output_via_the_attached_sink`)
/// pokes `Peripherals::log_sink` directly instead and says why in its
/// own doc; this fixture goes one step further because a hand-rolled
/// `log()` call is cheap here (no toolchain needed, same as every
/// other fixture in this module).
pub fn logging_cart() -> Vec<u8> {
    // 15 instructions precede the message text, so it lands at byte
    // offset 15 * 4 = 60 -- comfortably inside a 12-bit signed `addi`
    // immediate (max 2047).
    const STR_OFFSET: i32 = 15 * 4;
    let xip_base_hi20 = ggo_emu_core::sandbox::XIP_BASE >> 12;

    let body: Vec<u32> = vec![
        lui(A0, xip_base_hi20),             // a0 = XIP_BASE (hi bits)
        addi_reg(A0, A0, STR_OFFSET),       // a0 += offset of the message
        addi(A1, LOG_MESSAGE.len() as i32), // a1 = message length
        addi(A7, SYS_LOG),                  //
        ECALL,                              // log(a0, a1)
        addi(A0, GREEN as i32),             // backdrop colour
        addi(A1, 0),                        //
        addi(A2, 0),                        //
        addi(A7, SYS_SET_BACKDROP),         //
        ECALL,                              //
        addi(A7, SYS_PRESENT),              // loop:
        ECALL,                              //
        addi(A7, SYS_VSYNC_WAIT),           //
        ECALL,                              //
        jal_x0(-16),                        // back to `loop`
    ];
    debug_assert_eq!(body.len(), 15, "STR_OFFSET assumes exactly 15 instructions");
    let mut body: Vec<u8> = body.iter().flat_map(|w| w.to_le_bytes()).collect();
    body.extend_from_slice(LOG_MESSAGE.as_bytes());

    let mut h = [0u8; HEADER_LEN];
    h[0x00..0x04].copy_from_slice(&MAGIC);
    h[0x04..0x06].copy_from_slice(&SUPPORTED_HEADER_VERSION.to_le_bytes());
    h[0x06..0x08].copy_from_slice(&0u16.to_le_bytes()); // required_abi
    h[0x08..0x08 + LOGGING_CART_TITLE.len()].copy_from_slice(LOGGING_CART_TITLE.as_bytes());
    h[0x28..0x2C].copy_from_slice(&0u32.to_le_bytes()); // entry_offset
    h[0x2C..0x30].copy_from_slice(&(body.len() as u32).to_le_bytes());
    h[0x30..0x34].copy_from_slice(&0u32.to_le_bytes()); // save_bytes
    h[0x34..0x38].copy_from_slice(&0u32.to_le_bytes()); // ram_needed
    h[0x38..0x3C].copy_from_slice(&0u32.to_le_bytes()); // flags
    let crc = crc32(&h[0x00..0x3C]);
    h[0x3C..0x40].copy_from_slice(&crc.to_le_bytes());

    let mut out = h.to_vec();
    out.extend_from_slice(&body);
    out
}

pub const ECHO_CART_TITLE: &str = "Echo Fix";

/// The `comm_recv` destination and `comm_send` source: the base of
/// the cart's own arena, which is the one region the sandbox lets a
/// cart both write (so `comm_recv` may fill it) and read (so
/// `comm_send` may frame it).
const ECHO_BUFFER: u32 = ggo_emu_core::sandbox::ARENA_BASE;

/// The capacity the cart offers `comm_recv`, and the wire's own
/// maximum: `ggo_wire::MAX_PAYLOAD`.
const ECHO_CAPACITY: i32 = 255;

/// A cart that echoes the link: every frame it asks for a datagram
/// and, if one arrived, sends the very same bytes straight back,
/// then waits for vsync.
///
/// ```text
///     a3 = ARENA_BASE                   ; the buffer, kept across ecalls
/// loop:
///     a0 = a3; a1 = 255; comm_recv()    ; a0 = payload length, 0 = nothing
///     if a0 <= 0 goto wait
///     a1 = a0; a0 = a3; comm_send()     ; the same bytes back
/// wait:
///     vsync_wait()                      ; frame boundary -- where the
///     j loop                            ; host's pump runs
/// ```
///
/// This is what makes `pump_link`'s CALL SITE
/// testable: the module's own tests drive the two halves directly,
/// but only a cart that really reads and writes its comm queues
/// exercises the frame boundary the driver pumps at.
pub fn comm_echo_cart() -> Vec<u8> {
    const A3: u32 = 13;
    let body: Vec<u32> = vec![
        lui(A3, ECHO_BUFFER >> 12), // a3 = the arena buffer
        addi_reg(A0, A3, 0),        // loop: a0 = buf
        addi(A1, ECHO_CAPACITY),    // a1 = capacity
        addi(A7, SYS_COMM_RECV),    //
        ECALL,                      // a0 = length (0 = nothing queued)
        bge(0, A0, 20),             // if a0 <= 0, skip to `wait`
        addi_reg(A1, A0, 0),        // a1 = length
        addi_reg(A0, A3, 0),        // a0 = buf
        addi(A7, SYS_COMM_SEND),    //
        ECALL,                      // comm_send(buf, length)
        addi(A7, SYS_VSYNC_WAIT),   // wait:
        ECALL,                      //
        jal_x0(-44),                // back to `loop`
    ];
    // `bge`'s +20 and `jal`'s -44 are byte offsets into this exact
    // sequence; an inserted instruction silently re-aims both.
    debug_assert_eq!(body.len(), 13, "the branch offsets assume 13 instructions");
    let body: Vec<u8> = body.iter().flat_map(|w| w.to_le_bytes()).collect();

    let mut h = [0u8; HEADER_LEN];
    h[0x00..0x04].copy_from_slice(&MAGIC);
    h[0x04..0x06].copy_from_slice(&SUPPORTED_HEADER_VERSION.to_le_bytes());
    h[0x06..0x08].copy_from_slice(&0u16.to_le_bytes()); // required_abi
    h[0x08..0x08 + ECHO_CART_TITLE.len()].copy_from_slice(ECHO_CART_TITLE.as_bytes());
    h[0x28..0x2C].copy_from_slice(&0u32.to_le_bytes()); // entry_offset
    h[0x2C..0x30].copy_from_slice(&(body.len() as u32).to_le_bytes());
    h[0x30..0x34].copy_from_slice(&0u32.to_le_bytes()); // save_bytes
    h[0x34..0x38].copy_from_slice(&0u32.to_le_bytes()); // ram_needed
    h[0x38..0x3C].copy_from_slice(&0u32.to_le_bytes()); // flags
    let crc = crc32(&h[0x00..0x3C]);
    h[0x3C..0x40].copy_from_slice(&crc.to_le_bytes());

    let mut out = h.to_vec();
    out.extend_from_slice(&body);
    out
}

/// Asset payload [`overrun_cart`] carries in its TOC.
/// `sandbox::plan` carves the read-only asset pools off the TOP of
/// the die and leaves the rest as arena, so a cart with assets is the
/// only kind whose arena ends below the die top -- with no assets the
/// arena IS the rest of the die and "past the arena" cannot be
/// expressed at all.
const OVERRUN_ASSET_BYTES: u32 = 64 * 1024;

/// Guest address [`overrun_cart`] stores to: the first byte above its
/// arena, which is the base of the pool the plan carved for
/// [`OVERRUN_ASSET_BYTES`] (granule-aligned, and 64 KiB is already a
/// whole number of granules, so this is exact).
pub const OVERRUN_ADDR: u32 =
    ggo_emu_core::sandbox::PSRAM_BASE + ggo_emu_core::sandbox::PSRAM_BYTES - OVERRUN_ASSET_BYTES;

/// A cart whose very first store runs off the end of its arena, then
/// -- if it somehow survives that -- paints green forever like
/// [`green_screen_cart`]. The green tail is the point: a run that
/// forgets to install the cart PMP does not fault at all, so the
/// missing sandbox shows up as frames arriving rather than as a
/// timeout.
pub fn overrun_cart() -> Vec<u8> {
    const TITLE: &str = "Overrun Fix";
    let body: Vec<u32> = vec![
        lui(A0, OVERRUN_ADDR >> 12), // a0 = first byte above the arena
        sw(A0, 0, 0),                // *a0 = 0 -- the pool is read-only
        addi(A0, GREEN as i32),      // backdrop colour
        addi(A1, 0),                 //
        addi(A2, 0),                 //
        addi(A7, SYS_SET_BACKDROP),  //
        ECALL,                       //
        addi(A7, SYS_PRESENT),       // loop:
        ECALL,                       //
        addi(A7, SYS_VSYNC_WAIT),    //
        ECALL,                       //
        jal_x0(-16),                 // back to `loop`
    ];
    let body: Vec<u8> = body.iter().flat_map(|w| w.to_le_bytes()).collect();

    // A one-entry GGO2 section (`ggo_asset_formats::Section`): 8-byte
    // header, one 13-byte TOC entry, then the blob itself -- which has
    // to really be there, since `Section::parse` validates every
    // entry's blob range and `pool_demand` charges the pools from the
    // bytes it finds, not from the declared length.
    let mut toc: Vec<u8> = Vec::with_capacity(OVERRUN_ASSET_BYTES as usize + 32);
    toc.extend_from_slice(b"GGO2");
    toc.extend_from_slice(&1u16.to_le_bytes()); // section version
    toc.extend_from_slice(&1u16.to_le_bytes()); // entry count
    toc.extend_from_slice(&0u32.to_le_bytes()); // path_hash -- never looked up
    toc.push(1); // kind Til: a RAM-pool kind
    toc.extend_from_slice(&0u32.to_le_bytes()); // blob offset
    toc.extend_from_slice(&OVERRUN_ASSET_BYTES.to_le_bytes());
    toc.resize(toc.len() + OVERRUN_ASSET_BYTES as usize, 0);

    let mut h = [0u8; HEADER_LEN];
    h[0x00..0x04].copy_from_slice(&MAGIC);
    h[0x04..0x06].copy_from_slice(&SUPPORTED_HEADER_VERSION.to_le_bytes());
    h[0x06..0x08].copy_from_slice(&0u16.to_le_bytes()); // required_abi
    h[0x08..0x08 + TITLE.len()].copy_from_slice(TITLE.as_bytes());
    h[0x28..0x2C].copy_from_slice(&0u32.to_le_bytes()); // entry_offset
    h[0x2C..0x30].copy_from_slice(&(body.len() as u32).to_le_bytes());
    h[0x30..0x34].copy_from_slice(&0u32.to_le_bytes()); // save_bytes
    h[0x34..0x38].copy_from_slice(&0u32.to_le_bytes()); // ram_needed
    h[0x38..0x3C].copy_from_slice(&FLAG_HAS_ASSET_TOC.to_le_bytes());
    let crc = crc32(&h[0x00..0x3C]);
    h[0x3C..0x40].copy_from_slice(&crc.to_le_bytes());

    let mut out = h.to_vec();
    out.extend_from_slice(&body);
    out.extend_from_slice(&toc);
    out
}
