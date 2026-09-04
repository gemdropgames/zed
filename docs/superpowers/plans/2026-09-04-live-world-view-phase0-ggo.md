# Live World View, Phase 0 (ggo prerequisites) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the UART comm link on ggo `main` and make the sandbox (cart-XIP) emulator speak `COMM_SEND`/`COMM_RECV`/`COMM_STATUS`, so a host can exchange `CHANNEL_APP` datagrams with a cart running in ZedGG's in-process emulator exactly as it would over a real link cable.

**Architecture:** Rebase the existing branch `worktree-uart-comm-link` (12 commits) onto `main` and merge it. Then give `ggo-emu-core`'s `Peripherals` the same RX decoder + APP queue firmware uses (`ggo_hal::comm::CommState`, a pure `no_std` struct) plus a TX byte buffer, and add three syscall arms to the sandbox dispatcher. A host injects wire bytes and drains wire bytes; nothing above the wire differs from hardware.

**Tech Stack:** Rust workspaces `firmware/`, `sdk/`, `tools/` in `~/projects/ggo`; crates `ggo-wire`, `ggo-hal`, `ggo-comm`, `ggo-emu-core`, `ggo-emu`; riscv32imc toolchain for the echo cart; `cargo test`.

**Spec:** `docs/superpowers/specs/2026-09-04-live-world-view-design.md` (in `~/projects/zed`), section "ggo changes (phase 0)". The comm link's own spec is `~/projects/ggo/docs/superpowers/specs/2026-08-12-uart-comm-design.md`.

## Global Constraints

- All work in `~/projects/ggo`. ZedGG builds against this checkout by path (`ggo-emu-core = { path = "../ggo/tools/ggo-emu-core" }`), so a broken `main` breaks zed; keep every commit green.
- Wire protocol is the merged branch's, unchanged: `0x00`, `0x9C` sentinel, COBS block of `channel u8 + payload (≤255) + CRC32 LE`, `0x00`. `CHANNEL_APP = 8`. Syscall numbers `COMM_SEND = 0x4D`, `COMM_RECV = 0x4E`, `COMM_STATUS = 0x4F`.
- Syscall return codes, identical to firmware's `firmware/system/src/cart.rs`: `COMM_SEND` → `0` ok, `-1` len > 255 or bad pointer, `-2` TX stalled (never happens in the emulator); `COMM_RECV` → payload length, `0` nothing queued, `-1` bad destination or cap too small (message dropped); `COMM_STATUS` → `rx_queued | queue_drops << 8 | frame_drops << 16 | other_drops << 24`.
- APP RX queue depth 4 (`ggo_hal::comm::APP_RX_QUEUE_DEPTH`); newest dropped when full.
- `Peripherals::log_sink` keeps raw text (the sandbox console reads it as lines); comm TX goes to a separate buffer.
- Gates per workspace: `cd tools && cargo test --workspace --all-targets && cargo build --workspace --release`; `cd firmware/ggo-hal && cargo test --features sim && cargo build --target riscv32imc-unknown-none-elf`; `cd firmware/system && cargo build --release`; `cd firmware/boot-rom && cargo build --release`; `cd sdk && cargo test`; `cd sdk/examples/comm-echo && cargo build --release`.
- Commit messages: short imperative subject, no AI trailers.
- Do not touch `~/.ggo/ggo` (the flash checkout); it is updated by pulling `main` after the merge.

---

## File structure

| Path | Responsibility |
|------|----------------|
| `tools/ggo-emu-core/Cargo.toml` | Add path deps `ggo-wire` (encoder) and `ggo-hal` (`comm::CommState`, default features = `no_std`, builds on host); dev-dep `ggo-comm` (host decoder for tests). |
| `tools/ggo-emu-core/src/peripherals.rs` | `comm: CommState`, `comm_tx: Vec<u8>`, `uart_inject`, `take_comm`. |
| `tools/ggo-emu-core/src/abi.rs` | `Syscall::{CommSend, CommRecv, CommStatus}` + `from_a7` arms. |
| `tools/ggo-emu-core/src/runtime.rs` | The three dispatcher arms + `handle_comm_recv`; unit tests beside the existing syscall tests. |
| `tools/ggo-emu/src/lib.rs` | Env-gated sandbox echo e2e beside the full-system one. |
| `docs/abi.md` | One paragraph: the sandbox emulator implements `COMM_*` natively. |

---

### Task 1: Rebase the comm branch onto main

**Files:**
- Modify (conflicts): `docs/abi.md`, `firmware/ggo-hal/src/perf.rs`, `firmware/ggo-hal/src/uart.rs`, `firmware/ggo-wire/src/lib.rs`, `firmware/system/Cargo.toml`, `firmware/system/src/cart.rs`, `firmware/system/src/main.rs`, `sdk/gemdrop-sdk/src/lib.rs`, `tools/Cargo.toml`, `tools/Cargo.lock`

**Interfaces:**
- Produces: on branch `uart-comm-link`, crates `ggo-wire` (`Message`, `encode_payload`, `parse_block`, `cobs_decode`, `channel::APP`), `ggo-comm` (`MessageReader::feed(&[u8]) -> Vec<LinkItem>`, `LinkItem::Message(Message)`), `ggo_hal::comm::{CommState, RxDecoder, APP_RX_QUEUE_DEPTH}` with `CommState::{new, feed(u8), pop_app() -> Option<Message>, status_word() -> u32}`, SDK `gemdrop_sdk::comm::{send, recv, status}` and `sys::{COMM_SEND, COMM_RECV, COMM_STATUS}`, `FullSystemBus::uart_inject`.

- [ ] **Step 1: Branch off the old branch, keep it as a backup**

```bash
cd ~/projects/ggo
git status --porcelain            # must be empty
git branch backup/uart-comm-link-pre-rebase worktree-uart-comm-link
git checkout -b uart-comm-link worktree-uart-comm-link
git rebase main
```

Expected: rebase stops on conflicts, at most in the ten files listed above, possibly over several of the 12 commits.

- [ ] **Step 2: Resolve each conflict by rule**

Rule for every file: keep `main`'s newer behaviour, re-apply the branch's *additions* on top of it. Specifically:

- `firmware/ggo-wire/src/lib.rs`: the branch REPLACES the legacy `PacketType` API with `Message`/`encode_payload`/`parse_block`; take the branch's file whole, then re-apply any `main`-side edits made after 2026-08-12 (`git log main --oneline -- firmware/ggo-wire` shows them; commit `5c709be2` documented the `std` feature gate, keep that note).
- `firmware/ggo-hal/src/uart.rs`: branch adds RX accessors (`rx_pop`, `rx_ready`, IRQ enable); `main` changed TX-side code. Keep both.
- `firmware/ggo-hal/src/perf.rs`: branch adds `FrameStat::to_le_bytes`/`from_le_bytes` and deletes `format_frame`; `main` (commit `06d6f202`) changed over-budget semantics (refresh period + jitter dead band). Keep `main`'s semantics, keep the branch's byte codec, drop `format_frame` only if nothing on `main` still calls it (`grep -rn format_frame firmware tools`).
- `firmware/system/src/cart.rs` and `main.rs`: branch adds the `COMM_*` syscall arms, `comm_pump` drain points (trap arm, `vsync_wait`, TX-spin) and the `WireSink`-based logging. Keep every `main`-side change to other arms and to boot ordering (`672f0048` fixed boot-rom stack + log flush ordering on the branch itself; if `main` moved the same code, prefer `main`'s placement and re-insert the pump calls at the same three points).
- `firmware/system/Cargo.toml`, `tools/Cargo.toml`, `tools/Cargo.lock`: union of dependency/member lists (`ggo-comm` joins `members`; `ggo-wire` joins system's deps). Regenerate the lock with `cargo update -w` inside `tools/` rather than hand-merging hunks.
- `sdk/gemdrop-sdk/src/lib.rs`: branch appends the comm section (`CommError`, `CommStatus`, `comm_send/recv/status`, `pub mod comm`) and the three `sys::` constants; `main` edited unrelated sections. Keep both.
- `docs/abi.md`: keep both sides' sections.

After each resolved commit:

```bash
git add -A && GIT_EDITOR=true git rebase --continue
```

- [ ] **Step 3: Run every workspace gate**

```bash
cd ~/projects/ggo/tools && cargo test --workspace --all-targets && cargo build --workspace --release
cd ~/projects/ggo/firmware/ggo-hal && cargo test --features sim && cargo build --target riscv32imc-unknown-none-elf
cd ~/projects/ggo/firmware/system && cargo build --release
cd ~/projects/ggo/firmware/boot-rom && cargo build --release
cd ~/projects/ggo/sdk && cargo test
cd ~/projects/ggo/sdk/examples/comm-echo && cargo build --release
```

Expected: all exit 0. Known pre-existing failures on `main` (2 `ggo-emu` fullsystem, 2 `ggo-server` api, per `docs/ggo/UPSTREAM.md` in zed) may remain; compare against `git stash; cargo test ...` on `main` before attributing them to the rebase. The branch's own tests must pass: `cargo test -p ggo-wire` (in `firmware/`), `cargo test -p ggo-comm` (in `tools/`), `cargo test --features sim comm::` (in `firmware/ggo-hal`), and `cargo test -p ggo-emu full_system_comm_echo -- --ignored` if the riscv toolchain and `mtools` are installed.

- [ ] **Step 4: Fix any fallout, commit**

Any fix goes in its own commit on `uart-comm-link`:

```bash
git commit -m "uart-comm: rebase fallout — <what>"
```

- [ ] **Step 5: Confirm ZedGG still builds against the rebased branch**

```bash
cd ~/projects/zed && cargo check -p ggo_emu_panel -p ggo_world_panel
```

Expected: exit 0 (the branch changes no `ggo-emu-core` public API yet).

---

### Task 2: Hardware parity gate (user-run)

**Files:** none

**Interfaces:**
- Consumes: Task 1's branch, flashed to the board.

- [ ] **Step 1: Build and flash the rebased firmware**

Hand these to the user (they own the board and the flash checkout):

```bash
cd ~/.ggo/ggo && git fetch ~/projects/ggo uart-comm-link:uart-comm-link && git checkout uart-comm-link
# then the usual flash: ZedGG "Flash" button, or
ggo-diag --project <an emerald project> --tty /dev/serial/by-id/<board> --skip-pnr
```

- [ ] **Step 2: Verify TX parity**

In `~/.ggo/diag/logs/<stamp>.log` (see memory note "Flash run log triage sources"): boot stages detected (`CHANNEL_BOOT`), heartbeat blocks stitched, `FRAME` rows landing in the perf database. Any `text fallback` lines beyond the boot ROM phase mean an unframed emitter survived the merge; find it with `grep -rn "put_str\|uart_write" firmware/system/src` and route it through `WireSink`.

- [ ] **Step 3: Verify RX (host → cart → host)**

```bash
cd ~/projects/ggo/sdk/examples/comm-echo && cargo build --release
cd ~/projects/ggo/tools && cargo run -p ggo-pack -- ../sdk/examples/comm-echo/target/riscv32imc-unknown-none-elf/release/comm-echo --out /tmp/comm-echo.cart
# copy the cart onto the card image and launch it, then:
cargo run -p ggo-diag -- tail --tty /dev/serial/by-id/<board> --send-app "ping"
```

Expected: `ggo-diag tail` prints an `APP` message with payload `ping` coming back. If `tail` has no `--send-app` flag on the branch, add one (a single `GgoLink::send_app` before the read loop) in a commit `ggo-diag: tail --send-app for link round trips`.

- [ ] **Step 4: Merge**

Only after the user confirms both checks:

```bash
cd ~/projects/ggo && git checkout main && git merge --ff-only uart-comm-link && git push origin main
cd ~/.ggo/ggo && git checkout main && git pull
```

---

### Task 3: Sandbox comm state on `Peripherals`

**Files:**
- Modify: `tools/ggo-emu-core/Cargo.toml`
- Modify: `tools/ggo-emu-core/src/peripherals.rs` (struct at ~line 34, `new` at ~line 94, `take_log` at ~line 119)

**Interfaces:**
- Consumes: `ggo_hal::comm::CommState` (Task 1).
- Produces:
  - `Peripherals::comm: ggo_hal::comm::CommState` (pub)
  - `Peripherals::uart_inject(&mut self, bytes: &[u8])` — feeds every byte to the decoder.
  - `Peripherals::take_comm(&mut self) -> Vec<u8>` — drains the encoded TX frames.
  - `Peripherals::comm_tx: Vec<u8>` (pub(crate)) — where the send arm appends.

- [ ] **Step 1: Add the dependencies**

In `tools/ggo-emu-core/Cargo.toml` under `[dependencies]`:

```toml
# The device-side RX decoder + APP queue, byte-for-byte the firmware's
# (`ggo_hal::comm` is pure `no_std` over ggo-wire; default features keep
# it free of the `sim` PSRAM model). Reused so the sandbox emulator can
# never disagree with the board about what a cart receives.
ggo-hal = { path = "../../firmware/ggo-hal", default-features = false }
ggo-wire = { path = "../../firmware/ggo-wire" }
```

Under `[dev-dependencies]`:

```toml
# Host-side decoder for asserting what the send arm put on the wire.
ggo-comm = { path = "../ggo-comm" }
```

Run: `cd ~/projects/ggo/tools && cargo check -p ggo-emu-core`
Expected: exit 0. If `ggo-hal` fails to build on the host with default features, the offending module is one that references riscv-only items outside a `cfg`; gate that item with `#[cfg(target_arch = "riscv32")]` in `ggo-hal` (commit separately: `ggo-hal: host-buildable with default features`).

- [ ] **Step 2: Write the failing tests** (append to the `tests` module at the bottom of `peripherals.rs`; create the module if there is none)

```rust
#[cfg(test)]
mod comm_tests {
    use super::Peripherals;

    fn wire(channel: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(ggo_wire::encode_payload(channel, payload, |b| out.push(b)));
        out
    }

    #[test]
    fn injected_app_frame_is_queued_for_the_cart() {
        let mut p = Peripherals::new(0, 0);
        p.uart_inject(&wire(ggo_wire::channel::APP, b"ping"));
        let msg = p.comm.pop_app().expect("one APP message queued");
        assert_eq!(msg.payload(), b"ping");
        assert!(p.comm.pop_app().is_none(), "queue drained");
    }

    #[test]
    fn injected_non_app_frame_is_counted_not_queued() {
        let mut p = Peripherals::new(0, 0);
        p.uart_inject(&wire(ggo_wire::channel::LOG, b"noise"));
        assert!(p.comm.pop_app().is_none());
        assert_eq!(p.comm.status_word() >> 24, 1, "other_drops == 1");
    }

    #[test]
    fn injection_may_be_split_across_calls() {
        let mut p = Peripherals::new(0, 0);
        let bytes = wire(ggo_wire::channel::APP, b"split");
        let (a, b) = bytes.split_at(3);
        p.uart_inject(a);
        assert!(p.comm.pop_app().is_none(), "frame incomplete");
        p.uart_inject(b);
        assert_eq!(p.comm.pop_app().unwrap().payload(), b"split");
    }

    #[test]
    fn take_comm_drains_the_tx_buffer_once() {
        let mut p = Peripherals::new(0, 0);
        p.comm_tx.extend_from_slice(b"abc");
        assert_eq!(p.take_comm(), b"abc");
        assert!(p.take_comm().is_empty());
    }
}
```

- [ ] **Step 3: Run the tests, verify they fail**

Run: `cd ~/projects/ggo/tools && cargo test -p ggo-emu-core comm_tests`
Expected: compile error, no field `comm` / no method `uart_inject`.

- [ ] **Step 4: Implement**

In the `Peripherals` struct, after `log_sink`:

```rust
    /// The comm link's device side: the same RX decoder + `CHANNEL_APP`
    /// queue the firmware runs (`ggo_hal::comm`), fed by
    /// [`Self::uart_inject`] and popped by the `COMM_RECV` syscall. Pub so
    /// a driver can inspect the queue in tests.
    pub comm: ggo_hal::comm::CommState,
    /// Wire bytes the `COMM_SEND` syscall has encoded (ggo-wire framed,
    /// `CHANNEL_APP`), drained by [`Self::take_comm`]. Separate from
    /// `log_sink`, which stays raw text for the sandbox console.
    pub(crate) comm_tx: Vec<u8>,
```

In `new`, after `log_sink: None,`:

```rust
            comm: ggo_hal::comm::CommState::new(),
            comm_tx: Vec::new(),
```

After `take_log`:

```rust
    /// Feed host-side wire bytes to the cart's comm link, exactly as the
    /// board's UART RX would: any framing, any split. Decoded
    /// `CHANNEL_APP` messages queue for `COMM_RECV`; everything else is
    /// dropped and counted, as on hardware.
    pub fn uart_inject(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.comm.feed(b);
        }
    }

    /// Drain the wire bytes `COMM_SEND` has produced since the last call.
    pub fn take_comm(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.comm_tx)
    }
```

- [ ] **Step 5: Run the tests, verify they pass**

Run: `cd ~/projects/ggo/tools && cargo test -p ggo-emu-core comm_tests`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
cd ~/projects/ggo && git add tools/ggo-emu-core/Cargo.toml tools/ggo-emu-core/src/peripherals.rs tools/Cargo.lock
git commit -m "emu-core: sandbox comm RX queue and TX buffer on Peripherals"
```

---

### Task 4: `COMM_SEND` / `COMM_RECV` / `COMM_STATUS` in the sandbox dispatcher

**Files:**
- Modify: `tools/ggo-emu-core/src/abi.rs` (enum at ~line 80, `from_a7` at ~line 135)
- Modify: `tools/ggo-emu-core/src/runtime.rs` (dispatcher arms near the `Log` arm ~line 269; helper after `handle_save_write`; tests in the `tests` module beside `log_syscall_writes_to_sink_when_attached` ~line 1320)
- Modify: `docs/abi.md`

**Interfaces:**
- Consumes: `Peripherals::{comm, comm_tx}` (Task 3); `read_guest_bytes(mmu, ptr, len) -> Option<Vec<u8>>` and `Mmu::write_u8` (existing).
- Produces: `Syscall::{CommSend, CommRecv, CommStatus}`; return codes per Global Constraints.

- [ ] **Step 1: Write the failing tests** (in `runtime.rs`'s `tests` module, next to the log test; `ecall`, `ARENA_BASE`, `sys` are already in scope there)

```rust
    fn wire(channel: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(ggo_wire::encode_payload(channel, payload, |b| out.push(b)));
        out
    }

    fn app_wire(payload: &[u8]) -> Vec<u8> {
        wire(ggo_wire::channel::APP, payload)
    }

    fn decode_app(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut reader = ggo_comm::MessageReader::default();
        reader
            .feed(bytes)
            .into_iter()
            .filter_map(|item| match item {
                ggo_comm::LinkItem::Message(m) if m.channel == ggo_wire::channel::APP => {
                    Some(m.payload().to_vec())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn comm_recv_returns_zero_when_nothing_is_queued() {
        let mut mmu = Mmu::new();
        let mut cpu = Cpu::new(ARENA_BASE);
        let mut p = Peripherals::new(0, 0);
        ecall(&mut cpu, &mut mmu, &mut p, sys::COMM_RECV as u32, ARENA_BASE, 255, 0);
        assert_eq!(cpu.regs[10], 0);
    }

    #[test]
    fn comm_recv_copies_the_oldest_app_message_into_the_arena() {
        let mut mmu = Mmu::new();
        let mut cpu = Cpu::new(ARENA_BASE);
        let mut p = Peripherals::new(0, 0);
        p.uart_inject(&app_wire(b"ping"));
        p.uart_inject(&app_wire(b"second"));
        ecall(&mut cpu, &mut mmu, &mut p, sys::COMM_RECV as u32, ARENA_BASE, 255, 0);
        assert_eq!(cpu.regs[10], 4, "payload length returned");
        let got: Vec<u8> = (0..4).map(|i| mmu.read_u8(ARENA_BASE + i).unwrap()).collect();
        assert_eq!(got, b"ping");
        ecall(&mut cpu, &mut mmu, &mut p, sys::COMM_RECV as u32, ARENA_BASE, 255, 0);
        assert_eq!(cpu.regs[10], 6, "FIFO order");
    }

    #[test]
    fn comm_recv_with_a_small_cap_drops_the_message_and_returns_minus_one() {
        let mut mmu = Mmu::new();
        let mut cpu = Cpu::new(ARENA_BASE);
        let mut p = Peripherals::new(0, 0);
        p.uart_inject(&app_wire(b"ping"));
        ecall(&mut cpu, &mut mmu, &mut p, sys::COMM_RECV as u32, ARENA_BASE, 2, 0);
        assert_eq!(cpu.regs[10] as i32, -1);
        ecall(&mut cpu, &mut mmu, &mut p, sys::COMM_RECV as u32, ARENA_BASE, 255, 0);
        assert_eq!(cpu.regs[10], 0, "the too-small message is gone, not requeued");
    }

    #[test]
    fn comm_recv_rejects_a_destination_outside_writable_ram_before_popping() {
        let mut mmu = Mmu::new();
        let mut cpu = Cpu::new(ARENA_BASE);
        let mut p = Peripherals::new(0, 0);
        p.uart_inject(&app_wire(b"ping"));
        // The last byte of the die is writable; one past it is not.
        let end = crate::sandbox::PSRAM_BASE + crate::sandbox::PSRAM_BYTES;
        ecall(&mut cpu, &mut mmu, &mut p, sys::COMM_RECV as u32, end - 2, 255, 0);
        assert_eq!(cpu.regs[10] as i32, -1);
        ecall(&mut cpu, &mut mmu, &mut p, sys::COMM_RECV as u32, ARENA_BASE, 255, 0);
        assert_eq!(cpu.regs[10], 4, "a bad pointer never costs the cart the message");
    }

    #[test]
    fn comm_send_frames_the_payload_on_the_app_channel() {
        let mut mmu = Mmu::new();
        let mut cpu = Cpu::new(ARENA_BASE);
        let mut p = Peripherals::new(0, 0);
        for (i, b) in b"pong".iter().enumerate() {
            mmu.write_u8(ARENA_BASE + i as u32, *b).unwrap();
        }
        ecall(&mut cpu, &mut mmu, &mut p, sys::COMM_SEND as u32, ARENA_BASE, 4, 0);
        assert_eq!(cpu.regs[10], 0);
        assert_eq!(decode_app(&p.take_comm()), vec![b"pong".to_vec()]);
        assert!(p.take_log().is_empty(), "comm TX never lands in the text log sink");
    }

    #[test]
    fn comm_send_rejects_an_oversize_or_unreadable_payload() {
        let mut mmu = Mmu::new();
        let mut cpu = Cpu::new(ARENA_BASE);
        let mut p = Peripherals::new(0, 0);
        ecall(&mut cpu, &mut mmu, &mut p, sys::COMM_SEND as u32, ARENA_BASE, 256, 0);
        assert_eq!(cpu.regs[10] as i32, -1, "len > MAX_PAYLOAD");
        let end = crate::sandbox::PSRAM_BASE + crate::sandbox::PSRAM_BYTES;
        ecall(&mut cpu, &mut mmu, &mut p, sys::COMM_SEND as u32, end - 2, 4, 0);
        assert_eq!(cpu.regs[10] as i32, -1, "range runs off the die");
        assert!(p.take_comm().is_empty(), "nothing went on the wire");
    }

    #[test]
    fn comm_status_packs_queue_depth_and_drop_counters() {
        let mut mmu = Mmu::new();
        let mut cpu = Cpu::new(ARENA_BASE);
        let mut p = Peripherals::new(0, 0);
        for _ in 0..5 {
            p.uart_inject(&app_wire(b"x"));
        }
        p.uart_inject(&wire(ggo_wire::channel::LOG, b"noise"));
        ecall(&mut cpu, &mut mmu, &mut p, sys::COMM_STATUS as u32, 0, 0, 0);
        let w = cpu.regs[10];
        assert_eq!(w & 0xFF, 4, "rx_queued == depth");
        assert_eq!((w >> 8) & 0xFF, 1, "one queue drop");
        assert_eq!((w >> 16) & 0xFF, 0, "no frame drops");
        assert_eq!((w >> 24) & 0xFF, 1, "one other-channel drop");
    }
```

- [ ] **Step 2: Run the tests, verify they fail**

Run: `cd ~/projects/ggo/tools && cargo test -p ggo-emu-core comm_`
Expected: `COMM_RECV` returns garbage (unknown syscall path) or compile errors on missing `Syscall` variants; every test red.

- [ ] **Step 3: Implement**

`abi.rs`, in the `Syscall` enum after `Log = sys::LOG,`:

```rust
    // comm link (uart-comm spec): host <-> cart datagrams on CHANNEL_APP
    CommSend = sys::COMM_SEND,
    CommRecv = sys::COMM_RECV,
    CommStatus = sys::COMM_STATUS,
```

and in `from_a7` after `sys::LOG => Log,`:

```rust
            sys::COMM_SEND => CommSend,
            sys::COMM_RECV => CommRecv,
            sys::COMM_STATUS => CommStatus,
```

`runtime.rs`, dispatcher arms after the `Log` arm:

```rust
        // comm_send(ptr, len) -> 0, or -1 for len > MAX_PAYLOAD / an
        // unreadable range. The board's -2 (TX stalled) cannot happen
        // here: the host drains `comm_tx` without a FIFO in the way.
        Some(Syscall::CommSend) => {
            let ret = if a1 as usize > ggo_wire::MAX_PAYLOAD {
                -1
            } else {
                match read_guest_bytes(mmu, a0, a1) {
                    Some(bytes) => {
                        ggo_wire::encode_payload(ggo_wire::channel::APP, &bytes, |b| {
                            p.comm_tx.push(b)
                        });
                        0
                    }
                    None => -1,
                }
            };
            cpu.write_reg(10, ret as u32);
            EcallOutcome::Continue
        }
        // comm_recv(buf, cap) -> payload length, 0 when nothing is queued,
        // -1 for a bad destination or a cap too small (the message is
        // dropped in that case, never requeued; see the SDK doc).
        Some(Syscall::CommRecv) => {
            let ret = handle_comm_recv(mmu, p, a0, a1);
            cpu.write_reg(10, ret as u32);
            EcallOutcome::Continue
        }
        Some(Syscall::CommStatus) => {
            cpu.write_reg(10, p.comm.status_word());
            EcallOutcome::Continue
        }
```

Helper after `handle_save_write`:

```rust
/// `comm_recv(buf, cap)`: the firmware's rule, in order. A destination
/// outside the cart's arena (firmware's `arena_range_ok`) is rejected
/// BEFORE anything is popped, so a pointer bug never costs the cart a
/// message; a cap smaller than the queued payload pops and drops it,
/// because `comm_recv` never blocks on a bigger buffer.
fn handle_comm_recv(mmu: &mut Mmu, p: &mut Peripherals, buf: u32, cap: u32) -> i32 {
    let in_arena = buf >= crate::sandbox::ARENA_BASE
        && buf
            .checked_add(cap)
            .is_some_and(|end| end <= mmu.plan.arena_end());
    if !in_arena {
        return -1;
    }
    let Some(msg) = p.comm.pop_app() else {
        return 0;
    };
    let payload = msg.payload();
    if payload.len() > cap as usize {
        return -1;
    }
    for (i, &byte) in payload.iter().enumerate() {
        if mmu.write_u8(buf.wrapping_add(i as u32), byte).is_err() {
            return -1;
        }
    }
    payload.len() as i32
}
```

`docs/abi.md`, at the end of the comm-link section:

```
The sandbox emulator (`ggo-emu <cart>` and the ZedGG emulator pane) implements
these three syscalls natively rather than through GemOS: a host injects wire
bytes with `Peripherals::uart_inject` and drains the cart's frames with
`Peripherals::take_comm`. Return codes are the firmware's, except that `-2`
(TX stalled) cannot occur.
```

- [ ] **Step 4: Run the tests, verify they pass**

Run: `cd ~/projects/ggo/tools && cargo test -p ggo-emu-core`
Expected: the 7 new tests pass and the existing suite is unchanged.

- [ ] **Step 5: Clippy + full tools gate**

Run: `cd ~/projects/ggo/tools && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace --all-targets`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
cd ~/projects/ggo && git add tools/ggo-emu-core/src/abi.rs tools/ggo-emu-core/src/runtime.rs docs/abi.md
git commit -m "emu-core: COMM_SEND/RECV/STATUS in the sandbox dispatcher"
```

---

### Task 5: Sandbox echo round trip with the real `comm-echo` cart

**Files:**
- Modify: `tools/ggo-emu/src/lib.rs` (tests module, beside `full_system_comm_echo_cart_echoes_injected_app_message` ~line 1494)

**Interfaces:**
- Consumes: `Peripherals::{uart_inject, take_comm}` (Task 3), the dispatcher arms (Task 4), `sdk/examples/comm-echo` (Task 1), `ggo_emu_core::run::run_until_event`, `ggo_emu_core::sandbox`, `ggo_emu_core::cart::Cart`.

- [ ] **Step 1: Write the failing test**

Reuse the full-system echo test's gating and cart-build helper (read that test first; it builds `sdk/examples/comm-echo` with the riscv toolchain and packs it — call the same helper, or lift it into a `fn build_comm_echo_cart(repo: &Path) -> Option<PathBuf>` shared by both tests). Then:

```rust
    /// The sandbox twin of the full-system echo test: same cart, no GemOS.
    /// Proves the `COMM_*` arms in `ggo_emu_core::runtime` agree with the
    /// firmware's from a host's point of view — inject one framed APP
    /// message, get the identical payload back on the wire.
    #[test]
    #[ignore = "needs the riscv toolchain to build sdk/examples/comm-echo"]
    fn sandbox_comm_echo_cart_echoes_injected_app_message() {
        use ggo_emu_core::cart::Cart;
        use ggo_emu_core::cpu::Cpu;
        use ggo_emu_core::mmu::Mmu;
        use ggo_emu_core::peripherals::Peripherals;
        use ggo_emu_core::run::{run_until_event, FrameEvent};
        use ggo_emu_core::sandbox;

        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Some(cart_path) = build_comm_echo_cart(&repo) else {
            eprintln!("skipping: comm-echo cart could not be built");
            return;
        };
        let bytes = std::fs::read(&cart_path).unwrap();
        let cart = Cart::parse(&bytes).unwrap();

        let plan = sandbox::plan(
            sandbox::ARENA_MAX_LEN,
            0,
            0,
            cart.header.ram_needed.max(sandbox::MIN_ARENA),
        );
        let mut mmu = Mmu::with_plan(plan);
        assert!(mmu.load_cart_body(&cart.body));
        let mut cpu = Cpu::new(sandbox::XIP_BASE.wrapping_add(cart.header.entry_offset));
        ggo_emu_core::cpu::enter_sandbox(&mut cpu, &plan);
        let mut p = Peripherals::new(0, cart.header.save_bytes);
        p.log_sink = Some(Vec::new());

        let mut wire = Vec::new();
        assert!(ggo_wire::encode_payload(ggo_wire::channel::APP, b"ping", |b| wire.push(b)));

        let mut frames = 0;
        let mut echoed = Vec::new();
        let mut reader = ggo_comm::MessageReader::default();
        while frames < 10 {
            match run_until_event(&mut cpu, &mut mmu, &mut p, 2_000_000, false).0 {
                FrameEvent::Vsync(_) => {
                    frames += 1;
                    if frames == 2 {
                        p.uart_inject(&wire);
                    }
                    for item in reader.feed(&p.take_comm()) {
                        if let ggo_comm::LinkItem::Message(m) = item {
                            if m.channel == ggo_wire::channel::APP {
                                echoed.push(m.payload().to_vec());
                            }
                        }
                    }
                }
                FrameEvent::Budget => {}
                other => panic!("cart stopped early: {other:?}; log: {}",
                    String::from_utf8_lossy(&p.take_log())),
            }
        }
        assert_eq!(echoed, vec![b"ping".to_vec()]);
    }
```

If `FrameEvent`'s variants differ from `Vsync`/`Budget` in this checkout, match the names used by `ggo_emu_core::run::FrameEvent` (`grep -n "pub enum FrameEvent" -A12 tools/ggo-emu-core/src/run.rs`).

- [ ] **Step 2: Run it, verify it fails**

Run: `cd ~/projects/ggo/tools && cargo test -p ggo-emu sandbox_comm_echo -- --ignored`
Expected: fails on the missing `build_comm_echo_cart` helper (or, once lifted, passes only after Task 4 is in place).

- [ ] **Step 3: Lift the cart-build helper if needed, make it pass**

Run: `cd ~/projects/ggo/tools && cargo test -p ggo-emu comm_echo -- --ignored`
Expected: both echo tests pass (full-system and sandbox).

- [ ] **Step 4: Commit**

```bash
cd ~/projects/ggo && git add tools/ggo-emu/src/lib.rs
git commit -m "emu: sandbox comm echo e2e beside the full-system one"
```

---

### Task 6: Merge and refresh the flash checkout

**Files:** none

- [ ] **Step 1: Final gates on the branch**

```bash
cd ~/projects/ggo/tools && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace --all-targets && cargo build --workspace --release
cd ~/projects/zed && cargo check -p ggo_emu_panel
```

Expected: exit 0 everywhere.

- [ ] **Step 2: Merge to main, update the flash checkout**

```bash
cd ~/projects/ggo && git checkout main && git merge --ff-only uart-comm-link && git push origin main
cd ~/.ggo/ggo && git pull
```

Phase 1 (emerald link runtime, user systems, `editor_systems` scaffolding, `.ggo` viewer artifact, `[[background]]` format, `emerald-editor-link` crate) gets its own plan once this is on `main`; its cart-side tests need `gemdrop_sdk::comm` from the merged SDK.
