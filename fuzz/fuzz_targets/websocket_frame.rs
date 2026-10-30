#![no_main]
use lattice_api::websocket::{
    Config, Connection, Event, Opcode, UpgradeConfig, decode_client_frame, decode_server_frame,
    encode_client_frame, encode_frame, parse_upgrade,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // A client frame that decodes re-encodes to exactly the bytes it came from (one encoding per
    // frame: shortest length, mask key read from the frame), and is incomplete one byte short.
    if let Ok(Some((frame, used))) = decode_client_frame(data, 1 << 20) {
        assert!(used <= data.len());
        let key_at = match data.get(1).map(|b| b & 0x7f) {
            Some(126) => 4,
            Some(127) => 10,
            _ => 2,
        };
        let key: [u8; 4] = data[key_at..key_at + 4].try_into().expect("four mask bytes");
        let mut out = Vec::new();
        encode_client_frame(frame.fin, frame.opcode, &frame.payload, key, &mut out)
            .expect("a decoded frame encodes");
        assert_eq!(out, &data[..used]);
        assert_eq!(decode_client_frame(&data[..used - 1], 1 << 20), Ok(None));
    }
    // Server frames: the same round trip without a mask.
    if let Ok(Some((frame, used))) = decode_server_frame(data, 1 << 20) {
        let mut out = Vec::new();
        encode_frame(frame.fin, frame.opcode, &frame.payload, &mut out).expect("a decoded frame encodes");
        assert_eq!(out, &data[..used]);
        assert!(!frame.opcode.is_control() || frame.payload.len() <= 125);
        assert!(frame.opcode != Opcode::Close || frame.fin);
    }
    // A tiny cap never panics.
    let _ = decode_client_frame(data, 8);

    // The whole connection, with small limits so the caps are reached: as one stream, then fed one
    // byte at a time through a caller-side buffer.
    let cfg = Config { max_frame_payload: 64, max_message: 96, ..Config::default() };
    let mut conn = Connection::new(cfg.clone(), 0);
    let mut at = 0;
    let mut now = 0;
    loop {
        now += 1;
        let step = conn.receive(&data[at..], now);
        at += step.consumed;
        assert!(at <= data.len());
        if let Some(Event::ProtocolError { code, .. }) = step.event {
            assert!(matches!(code, 1002 | 1007 | 1009));
        }
        if step.consumed == 0 && step.event.is_none() {
            break;
        }
    }
    let _ = conn.tick(1 << 40);
    let mut out = Vec::new();
    conn.drain_output(&mut out);
    assert!(out.len() <= 2 + 125 + 2 + 8 + 2 + 125);

    let mut conn = Connection::new(cfg, 0);
    let mut buf: Vec<u8> = Vec::new();
    for b in data.iter().take(2048) {
        buf.push(*b);
        loop {
            let step = conn.receive(&buf, 1);
            buf.drain(..step.consumed);
            if step.consumed == 0 && step.event.is_none() {
                break;
            }
        }
        if conn.is_done() {
            break;
        }
        assert!(buf.len() <= 14 + 64);
    }

    // The upgrade request: a decision or a wait, and the head length lies within the input.
    if let Ok(Some((_, used))) = parse_upgrade(data, &UpgradeConfig::default()) {
        assert!(used <= data.len());
    }
});
