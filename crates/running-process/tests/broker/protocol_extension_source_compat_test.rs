//! External-consumer source compatibility for protocol type extensions (#1149).

#![cfg(feature = "client")]

use running_process::broker::protocol::{Endpoint, EndpointNameError, Frame, FrameKind};
use running_process::broker::protocol_v2::SessionStart;
use running_process::EnvironmentPolicy;

#[test]
fn session_start_policy_builder_needs_no_extension_trait_import() {
    let start = SessionStart::from_current_process("rustc", ["--version"], "work")
        .with_environment_policy(EnvironmentPolicy::Inherit);

    assert_eq!(start.environment_policy, 1);
    assert!(!start.clear_inherited_env);
}

#[test]
fn v1_frame_and_endpoint_extensions_need_no_extension_trait_import() {
    let mut request = Frame::request(0x7A63, b"ping".to_vec()).with_request_id(42);
    request.traceparent = "00-abc-def-01".to_owned();
    request.tracestate = "vendor=1".to_owned();
    let response = Frame::response_to(&request, b"pong".to_vec());

    assert_eq!(request.kind, FrameKind::Request as i32);
    assert_eq!(response.kind, FrameKind::Response as i32);
    assert_eq!(response.payload_protocol, request.payload_protocol);
    assert_eq!(response.request_id, 42);
    assert_eq!(response.traceparent, request.traceparent);
    assert_eq!(response.tracestate, request.tracestate);

    let pipe = Endpoint::windows_pipe("svc", "svc-pipe").expect("bare pipe name");
    assert_eq!(pipe.namespace_id, "svc");
    assert_eq!(pipe.path, "svc-pipe");
    assert_eq!(
        Endpoint::windows_pipe("svc", r"\\.\pipe\svc-pipe"),
        Err(EndpointNameError::PrefixedPipeName {
            got: r"\\.\pipe\svc-pipe".to_owned(),
        })
    );

    let socket = Endpoint::unix_socket("svc", "/tmp/svc.sock").expect("socket path");
    assert_eq!(socket.namespace_id, "svc");
    assert_eq!(socket.path, "/tmp/svc.sock");
}
