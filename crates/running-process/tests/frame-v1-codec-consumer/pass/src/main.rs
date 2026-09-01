running_process::register_payload_protocol! {
    pub const EXTERNAL_CONSUMER_PROTOCOL: u32 = 0xF412;
}

fn main() {
    let frame = running_process::frame_v1::Frame::request(
        EXTERNAL_CONSUMER_PROTOCOL,
        b"external consumer".to_vec(),
    );
    let _wire = running_process::frame_v1::encode_framed(&frame).expect("encode frame");
}
