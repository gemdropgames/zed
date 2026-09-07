//! A REAL viewer cart on the far end of the panel's own `LinkEndpoint`.
//!
//! `install` wires the editor runtime's schedule -- the link pumps, the
//! built-in edit systems and the two mode tables -- so one
//! [`CartHarness::frame`] is exactly what a cart's run loop does. The
//! journey tests drive the panel's real mouse and keymap handlers against
//! this, rather than against hand-rolled datagrams: everything between the
//! click and the document op is the shipping code on both sides.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use emerald_editor_runtime::link::{CartLink, SystemTable};
use emerald_editor_runtime::{Mailbox, install, mailbox_ptr, set_link};

/// The cart's half of the panel's endpoint. The host queue holds ggo-wire
/// FRAMES (the world panel's one framing site is `send_app`), so the cart
/// side has to run the same `MessageReader` the emulator thread runs over
/// the link -- decoding to APP payloads is what makes this the real wire
/// rather than a shortcut around it.
struct EndpointCartLink {
    endpoint: Arc<ggo_common::LinkEndpoint>,
    /// The wire and the cart's receive queue behind it, shared with the
    /// harness -- see [`Inbound`].
    inbound: Arc<Mutex<Inbound>>,
    /// While set, every cart -> host BLOB datagram is dropped on the
    /// floor. Only the blob kinds: the rows, the selection and the
    /// greeting still flow, so the session stays live and a save that
    /// asked for bytes simply never gets them
    /// ([`CartHarness::drop_cart_blobs`]).
    drop_blobs: Arc<AtomicBool>,
}

/// How many reassembled APP messages the cart holds at once, and what
/// happens past that: the firmware's own ring
/// (`ggo-hal/src/comm.rs`'s `APP_RX_QUEUE_DEPTH`, `CommState::enqueue`),
/// which is four deep and drops the NEWEST arrival when it is full.
/// Named here rather than imported: the firmware crate is not a
/// dependency of the editor, and this is a protocol constant.
const APP_RX_QUEUE_DEPTH: usize = 4;

/// The host -> cart direction, modelled as the firmware models it.
///
/// A datagram the host sends is on the WIRE until the cart's receive queue
/// takes it in, and that queue is four deep. The cart drains it from
/// `pump_inbound`, which stops at the first datagram that COMMITS into the
/// mailbox's one command slot -- so a burst of commands leaves the queue
/// full with the rest of the burst still arriving, and those are the ones
/// the firmware drops. Modelled at the frame boundary
/// ([`Inbound::settle`]): everything still on the wire when the cart's
/// frame ends arrived while the queue was full.
///
/// Shared between the link (which the runtime owns) and the harness, which
/// is what lets the frame boundary reach it at all.
#[derive(Default)]
struct Inbound {
    /// The cart side of the wire format -- the world panel frames every
    /// payload, so this is the same `MessageReader` the emulator thread
    /// runs.
    reader: ggo_comm::MessageReader,
    /// Decoded payloads the receive queue has not taken in yet.
    wire: VecDeque<Vec<u8>>,
    /// The cart's APP receive queue, oldest first.
    queue: VecDeque<Vec<u8>>,
    /// Datagrams lost because they arrived with the queue full.
    drops: usize,
}

impl Inbound {
    /// Decode everything the host has queued since the last look onto the
    /// wire, then hand the receive queue as much of it as it can hold.
    fn deliver(&mut self, endpoint: &ggo_common::LinkEndpoint) {
        for frame in endpoint.take_outbound() {
            for item in self.reader.feed(&frame) {
                if let ggo_comm::LinkItem::Message(message) = item {
                    self.wire.push_back(message.payload().to_vec());
                }
            }
        }
        while self.queue.len() < APP_RX_QUEUE_DEPTH {
            let Some(next) = self.wire.pop_front() else {
                return;
            };
            self.queue.push_back(next);
        }
    }

    /// End of one cart frame: what is still on the wire is what arrived
    /// while the queue was full, which is exactly what the firmware drops.
    fn settle(&mut self, endpoint: &ggo_common::LinkEndpoint) {
        self.deliver(endpoint);
        self.drops += self.wire.len();
        self.wire.clear();
    }
}

impl CartLink for EndpointCartLink {
    fn recv(&mut self, buf: &mut [u8]) -> usize {
        let mut inbound = self
            .inbound
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inbound.deliver(&self.endpoint);
        let Some(datagram) = inbound.queue.pop_front() else {
            return 0;
        };
        // A datagram the cart's buffer cannot hold is a wire bug, not a
        // short read: truncating it silently would hand the cart half a
        // message and let the test pass on the half.
        debug_assert!(
            datagram.len() <= buf.len(),
            "a {}-byte datagram does not fit the cart's {}-byte buffer",
            datagram.len(),
            buf.len()
        );
        let taken = datagram.len().min(buf.len());
        buf[..taken].copy_from_slice(&datagram[..taken]);
        taken
    }

    fn send(&mut self, payload: &[u8]) -> bool {
        // `0x90 CartBlobBegin`, `0x91 CartBlobChunk`, `0x92 CartBlobEnd`.
        // `true` all the same: the cart handed the bytes to the wire, and
        // a wire that then lost them is not something it can tell.
        if self.drop_blobs.load(Ordering::SeqCst) && matches!(payload.first(), Some(0x90..=0x92)) {
            return true;
        }
        self.endpoint.push_inbound(payload.to_vec());
        // What the emulator thread does once per presented frame: the
        // panel's poll loop only wakes on a tick (or the 250 ms backstop,
        // which a test clock never reaches).
        self.endpoint.tick();
        true
    }
}

/// `install` and the systems it schedules drive the runtime's ONE static
/// mailbox, so two harnesses must never be live at once. Every harness
/// holds this for its lifetime and starts from a blank mailbox.
static MAILBOX_LOCK: Mutex<()> = Mutex::new(());

/// One viewer cart running the real editor schedule against `endpoint`.
pub(crate) struct CartHarness {
    world: emerald_core::World,
    schedule: emerald_core::Schedule,
    endpoint: Arc<ggo_common::LinkEndpoint>,
    /// Re-published under a fresh number every frame rather than rebuilt:
    /// only the frame NUMBER is what takes the panel off the boot screen
    /// and moves its cart clock, and a `RenderImage` per frame would leak
    /// an atlas entry per frame (see `LinkEndpoint::frame`).
    picture: Arc<gpui::RenderImage>,
    presented: u32,
    /// Shared with the link: see [`Self::drop_cart_blobs`].
    drop_blobs: Arc<AtomicBool>,
    /// Shared with the link: the wire and the four-deep receive queue
    /// behind it ([`Inbound`]).
    inbound: Arc<Mutex<Inbound>>,
    /// Held for the harness's lifetime -- see [`MAILBOX_LOCK`]. Last
    /// field, so it is dropped last: the world and schedule that run
    /// against the static mailbox must be gone before the next harness
    /// may take it.
    _guard: MutexGuard<'static, ()>,
}

impl CartHarness {
    /// A cart wired to `endpoint`, offering `edit_systems` in Edit and
    /// `game_systems` in Play -- the same two tables a real cart hands
    /// `install`.
    pub fn new(
        endpoint: Arc<ggo_common::LinkEndpoint>,
        edit_systems: SystemTable,
        game_systems: SystemTable,
    ) -> Self {
        // A test that panicked while holding the lock poisons it; the
        // mailbox is blanked on entry anyway, so the next harness can take
        // it over regardless.
        let guard = MAILBOX_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Sound because of the lock: nothing else touches the static while
        // this harness holds it.
        unsafe { *mailbox_ptr() = Mailbox::new() };
        let mut world = emerald_core::World::new();
        let mut schedule = emerald_core::Schedule::new();
        install(
            &mut world,
            &mut schedule,
            emerald_world::SceneRegistry::with_builtins(),
            game_systems,
            edit_systems,
        );
        let drop_blobs = Arc::new(AtomicBool::new(false));
        let inbound = Arc::new(Mutex::new(Inbound::default()));
        assert!(
            set_link(
                &mut world,
                Box::new(EndpointCartLink {
                    endpoint: endpoint.clone(),
                    inbound: inbound.clone(),
                    drop_blobs: drop_blobs.clone(),
                }),
            ),
            "install left an editor runtime to swap the link into"
        );
        let picture = ggo_common::to_render_image(&vec![0u8; 320 * 240 * 4], 320, 240)
            .expect("a 320x240 RGBA buffer");
        CartHarness {
            world,
            schedule,
            endpoint,
            picture,
            presented: 0,
            drop_blobs,
            inbound,
            _guard: guard,
        }
    }

    /// How many host -> cart datagrams the cart's four-deep receive queue
    /// has lost -- see [`Inbound`]. Nonzero means the host sent a burst it
    /// did not pace, and every datagram past the fourth is one the cart
    /// never saw and cannot ask for again.
    pub fn dropped_datagrams(&self) -> usize {
        self.inbound
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drops
    }

    /// Drop every cart -> host blob from here on: the world snapshot and
    /// the layer readbacks a save asks for never arrive, which is the one
    /// failure the host cannot tell from a slow cart until the deadline.
    pub fn drop_cart_blobs(&self, dropping: bool) {
        self.drop_blobs.store(dropping, Ordering::SeqCst);
    }

    /// The cart's OWN copy of background slot `slot`: the source cells
    /// its layer holds, which is what a `ReadLayer` hands back. `None`
    /// until a layer has been loaded into the slot.
    pub fn layer_cells(&self, slot: usize) -> Option<(u16, u16, Vec<u16>)> {
        let mut buf = [0u16; emerald_editor_runtime::MAX_LAYER_CELLS];
        let (w, h, len) = emerald_editor_runtime::layer_cells(slot, &mut buf)?;
        Some((w, h, buf.get(..len)?.to_vec()))
    }

    /// One cart frame and the ONE host poll that follows it -- the poll
    /// loop wakes on the presented frame, which is the emulator's own
    /// tick, and it must wake exactly once per cart frame: two polls
    /// around one frame drain two pointer samples into it, and a press
    /// and the move after it arriving together is a press the cart never
    /// sees an edge for.
    ///
    /// So an input queued now goes on the wire at the END of this frame
    /// and the cart reads it in the NEXT one: a journey allows two frames
    /// per gesture step, exactly as the real link does.
    pub fn frame(&mut self, cx: &mut gpui::VisualTestContext) {
        self.schedule.run(&mut self.world);
        // The cart has stopped reading for this frame, so whatever is
        // still on the wire arrived with its receive queue full.
        self.inbound
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .settle(&self.endpoint);
        self.present(cx);
    }

    pub fn frames(&mut self, count: usize, cx: &mut gpui::VisualTestContext) {
        for _ in 0..count {
            self.frame(cx);
        }
    }

    /// Publish the frame the emulator would have drawn and let the panel
    /// poll on it. Publishing at all is what takes the tab off its boot
    /// screen -- `live_loading_text` covers the canvas until the cart has
    /// both greeted AND presented something.
    fn present(&mut self, cx: &mut gpui::VisualTestContext) {
        self.presented = self.presented.wrapping_add(1);
        *self
            .endpoint
            .frame
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((self.presented, self.picture.clone()));
        self.endpoint.tick();
        cx.run_until_parked();
    }
}
