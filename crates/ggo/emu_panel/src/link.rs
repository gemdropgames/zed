//! The viewer link's emulator side: at every frame boundary, hand the
//! host's queued wire bytes to the cart's comm RX and turn the cart's
//! comm TX into decoded `CHANNEL_APP` payloads for the host. Everything
//! else the cart puts on the wire (LOG/TELEMETRY) is not the link's
//! business; `take_log` still feeds the console separately.
//!
//! Split in two halves because the stream side is where the bugs live: a
//! cart's `COMM_SEND` frame arrives at whatever frame boundary the host
//! happens to drain on, so a frame can straddle two pumps and the
//! [`ggo_comm::MessageReader`] -- not this module -- is what holds the
//! partial. [`pump_inbound`] takes the raw bytes, so that case is
//! testable without staging a torn write through the emulator's TX
//! buffer, which only ever hands out whole frames.

use ggo_common::LinkEndpoint;
use ggo_emu_core::peripherals::Peripherals;

/// One frame boundary's worth of link traffic, both directions.
pub fn pump_link(
    p: &mut Peripherals,
    endpoint: &LinkEndpoint,
    reader: &mut ggo_comm::MessageReader,
) {
    pump_outbound(p, endpoint);
    let tx = p.take_comm();
    pump_inbound(&tx, endpoint, reader);
}

/// Host -> cart: everything the host queued since the last boundary,
/// injected as if it had arrived on the board's UART.
fn pump_outbound(p: &mut Peripherals, endpoint: &LinkEndpoint) {
    for bytes in endpoint.take_outbound() {
        p.uart_inject(&bytes);
    }
}

/// Cart -> host: decode `tx` (the cart's comm TX bytes since the last
/// boundary) and publish the `CHANNEL_APP` payloads. `reader` carries any
/// frame left half-arrived by the previous call.
fn pump_inbound(tx: &[u8], endpoint: &LinkEndpoint, reader: &mut ggo_comm::MessageReader) {
    if tx.is_empty() {
        return;
    }
    for item in reader.feed(tx) {
        if let ggo_comm::LinkItem::Message(message) = item
            && message.channel == ggo_wire::channel::APP
        {
            endpoint.push_inbound(message.payload().to_vec());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggo_common::LinkEndpoint;
    use ggo_emu_core::cpu::Cpu;
    use ggo_emu_core::mmu::Mmu;
    use ggo_emu_core::peripherals::Peripherals;
    use ggo_emu_core::sandbox::ARENA_BASE;

    fn wire(channel: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(ggo_wire::encode_payload(channel, payload, |b| out.push(b)));
        out
    }

    fn app_wire(payload: &[u8]) -> Vec<u8> {
        wire(ggo_wire::channel::APP, payload)
    }

    /// What a cart's `comm_send(payload)` does, driven the way
    /// `ggo-emu-core`'s own runtime tests drive a syscall: the framed
    /// bytes land in the peripherals' comm TX buffer, which is exactly
    /// what a real run's frame boundary finds there.
    fn cart_comm_send(cpu: &mut Cpu, mmu: &mut Mmu, p: &mut Peripherals, payload: &[u8]) {
        for (i, b) in payload.iter().enumerate() {
            mmu.write_u8(ARENA_BASE + i as u32, *b).unwrap();
        }
        cpu.write_reg(17, gemdrop_sdk::sys::COMM_SEND as u32);
        cpu.write_reg(10, ARENA_BASE);
        cpu.write_reg(11, payload.len() as u32);
        cpu.write_reg(12, 0);
        ggo_emu_core::runtime::dispatch_ecall(cpu, mmu, p, cpu.pc, false);
        assert_eq!(cpu.regs[10], 0, "comm_send was refused");
    }

    #[test]
    fn outbound_wire_bytes_reach_the_carts_comm_queue() {
        let mut p = Peripherals::new(0, 0);
        let endpoint = LinkEndpoint::new();
        let mut reader = ggo_comm::MessageReader::default();
        endpoint.send_wire(app_wire(b"hello"));
        pump_link(&mut p, &endpoint, &mut reader);
        assert_eq!(p.comm.pop_app().unwrap().payload(), b"hello");
    }

    #[test]
    fn cart_app_frames_are_decoded_into_inbound_payloads() {
        let mut p = Peripherals::new(0, 0);
        let mut cpu = Cpu::new(ARENA_BASE);
        let mut mmu = Mmu::new();
        let endpoint = LinkEndpoint::new();
        let mut reader = ggo_comm::MessageReader::default();
        cart_comm_send(&mut cpu, &mut mmu, &mut p, b"pong");
        pump_link(&mut p, &endpoint, &mut reader);
        assert_eq!(endpoint.try_recv_inbound(), vec![b"pong".to_vec()]);
        assert!(
            p.take_log().is_empty(),
            "comm frames never reach the console's text log"
        );
    }

    #[test]
    fn frames_on_other_channels_are_not_the_links_business() {
        let endpoint = LinkEndpoint::new();
        let mut reader = ggo_comm::MessageReader::default();
        let mut tx = app_wire(b"pong");
        tx.extend(wire(ggo_wire::channel::LOG, b"noise"));
        // Both frames really do decode -- so what follows is the channel
        // filter's doing, not a reader that dropped the LOG frame anyway.
        let decoded = ggo_comm::MessageReader::default()
            .feed(&tx)
            .into_iter()
            .filter_map(|item| match item {
                ggo_comm::LinkItem::Message(m) => Some(m.channel),
                ggo_comm::LinkItem::Text(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            decoded,
            vec![ggo_wire::channel::APP, ggo_wire::channel::LOG]
        );
        pump_inbound(&tx, &endpoint, &mut reader);
        assert_eq!(endpoint.try_recv_inbound(), vec![b"pong".to_vec()]);
    }

    #[test]
    fn a_frame_split_across_two_pumps_still_decodes() {
        let endpoint = LinkEndpoint::new();
        let mut reader = ggo_comm::MessageReader::default();
        let tx = app_wire(b"split");
        let (head, tail) = tx.split_at(4);
        pump_inbound(head, &endpoint, &mut reader);
        assert!(endpoint.try_recv_inbound().is_empty());
        pump_inbound(tail, &endpoint, &mut reader);
        assert_eq!(endpoint.try_recv_inbound(), vec![b"split".to_vec()]);
    }
}
