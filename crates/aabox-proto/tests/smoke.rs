//! Smoke test: encode + decode a real AAP message through prost.
//! Proves the .proto → Rust pipeline works end-to-end.

use aabox_proto::messages::ServiceDiscoveryRequest;
use prost::Message;

#[test]
fn service_discovery_request_roundtrip() {
    let req = ServiceDiscoveryRequest {
        device_name: "AABox".to_string(),
        device_brand: "AABox".to_string(),
    };

    let mut buf = Vec::with_capacity(req.encoded_len());
    req.encode(&mut buf).expect("encode");

    let decoded = ServiceDiscoveryRequest::decode(buf.as_slice()).expect("decode");
    assert_eq!(decoded.device_name, "AABox");
    assert_eq!(decoded.device_brand, "AABox");
}
