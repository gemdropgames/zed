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
use ggo_emu_wasm::WasmEmu;

/// One frame boundary's worth of link traffic, both directions. A failure
/// is the emulator module's, and ends the run.
pub fn pump_link(
    emu: &mut WasmEmu,
    endpoint: &LinkEndpoint,
    reader: &mut ggo_comm::MessageReader,
) -> anyhow::Result<()> {
    // Host -> cart: everything the host queued since the last boundary,
    // injected as if it had arrived on the board's UART.
    for bytes in endpoint.take_outbound() {
        emu.uart_inject(&bytes)?;
    }
    let tx = emu.take_comm()?;
    pump_inbound(&tx, endpoint, reader);
    Ok(())
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
    use crate::drive::{fixture, tests_support::test_emulator};
    use ggo_common::LinkEndpoint;
    use ggo_emu_wasm::TurnEvent;

    fn wire(channel: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(ggo_wire::encode_payload(channel, payload, |b| out.push(b)));
        out
    }

    fn app_wire(payload: &[u8]) -> Vec<u8> {
        wire(ggo_wire::channel::APP, payload)
    }

    /// Both halves of [`pump_link`] against a real cart: the host's
    /// datagram reaches the echo cart's `comm_recv` and comes back out of
    /// the endpoint decoded, with nothing leaking into the text log.
    #[test]
    fn pump_link_carries_a_datagram_to_the_cart_and_back() {
        let mut emu = test_emulator()
            .start_cart(&fixture::comm_echo_cart(), 0, None)
            .expect("the echo cart loads");
        let endpoint = LinkEndpoint::new();
        let mut reader = ggo_comm::MessageReader::default();
        endpoint
            .send_app(b"hello")
            .expect("a five-byte payload fits");

        let mut inbound = Vec::new();
        for _ in 0..30 {
            loop {
                match emu.run_turn(0, 0) {
                    TurnEvent::Vsync(_) => break,
                    TurnEvent::Budget => {}
                    other => panic!("the echo cart stopped: {other:?}"),
                }
            }
            pump_link(&mut emu, &endpoint, &mut reader).expect("the pump succeeds");
            inbound.extend(endpoint.try_recv_inbound());
            if !inbound.is_empty() {
                break;
            }
        }
        assert_eq!(inbound, vec![b"hello".to_vec()]);
        assert!(
            emu.take_log().expect("the log drains").is_empty(),
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
