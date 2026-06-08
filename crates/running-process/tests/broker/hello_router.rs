#![cfg(feature = "client")]

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use prost::Message;
use running_process::broker::backend_handle::BackendHandle;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, BrokerIsolation, ErrorCode, Frame, FrameKind, Hello,
    PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    ensure_service_definition_dir, service_definition_path, BackendRegistry, BrokerInstanceKey,
    HelloRequest, HelloRouter, PeerIdentity, ServiceDefinitionLoader, SpawnBudgetConfig,
    SpawnCoordinator,
};

use crate::backend_handle_common::current_daemon;

fn absolute_paths() -> (String, String) {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().to_path_buf();
    (
        exe.to_string_lossy().into_owned(),
        dir.to_string_lossy().into_owned(),
    )
}

fn service_definition(isolation: BrokerIsolation) -> ServiceDefinition {
    let (binary_path, per_version_binary_dir) = absolute_paths();
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path,
        isolation: isolation as i32,
        explicit_instance: String::new(),
        per_version_binary_dir,
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn write_definition_for(root: &Path, service_name: &str, definition: &ServiceDefinition) {
    let path = service_definition_path(root, service_name).unwrap();
    fs::write(path, definition.encode_to_vec()).unwrap();
}

fn service_dir_with_definition(definition: &ServiceDefinition) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();
    write_definition_for(&root, "zccache", definition);
    tmp
}

fn hello(service_name: &str, wanted_version: &str) -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: service_name.into(),
        wanted_version: wanted_version.into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "req-router".into(),
        connection_id: 0,
        peer_pid: 0,
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn request(service_name: &str, wanted_version: &str) -> HelloRequest {
    HelloRequest {
        frame: Frame {
            envelope_version: 1,
            kind: FrameKind::Request as i32,
            payload_protocol: 0,
            payload: hello(service_name, wanted_version).encode_to_vec(),
            request_id: 7,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: String::new(),
            tracestate: String::new(),
        },
        hello: hello(service_name, wanted_version),
        peer: PeerIdentity {
            pid: 0,
            uid_or_sid: "test-peer".into(),
        },
    }
}

fn registry_with_backend(instance: BrokerInstanceKey) -> (BackendRegistry, String) {
    let daemon = current_daemon();
    let expected_pipe = daemon.ipc_endpoint.path.clone();
    let handle =
        BackendHandle::probe_with_service("zccache", "1.11.20", &daemon.ipc_endpoint, &daemon)
            .unwrap();
    let mut registry = BackendRegistry::new();
    registry.insert(instance, handle);
    (registry, expected_pipe)
}

fn reply_code(reply: &running_process::broker::protocol::HelloReply) -> ErrorCode {
    match reply.result.as_ref().unwrap() {
        HelloReplyResult::Refused(refused) => ErrorCode::try_from(refused.code).unwrap(),
        HelloReplyResult::Negotiated(negotiated) => {
            panic!("expected refusal, got negotiated {negotiated:?}")
        }
    }
}

#[test]
fn router_negotiates_registry_backend_for_loaded_service_definition() {
    let definition = service_definition(BrokerIsolation::SharedBroker);
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let (registry, expected_pipe) = registry_with_backend(BrokerInstanceKey::Shared);
    let router = HelloRouter::new(&loader, &registry);

    let reply = router.handle_request(&request("zccache", "1.11.20"));

    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.backend_pipe, expected_pipe);
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn router_rereads_service_definition_on_each_request() {
    let mut definition = service_definition(BrokerIsolation::SharedBroker);
    let tmp = service_dir_with_definition(&definition);
    let service_root = tmp.path().join("services");
    let loader = ServiceDefinitionLoader::new(&service_root);
    let (registry, _) = registry_with_backend(BrokerInstanceKey::Shared);
    let router = HelloRouter::new(&loader, &registry);

    assert!(matches!(
        router.handle_request(&request("zccache", "1.11.20")).result,
        Some(HelloReplyResult::Negotiated(_))
    ));

    definition.min_version = "1.12.0".into();
    write_definition_for(&service_root, "zccache", &definition);

    let reply = router.handle_request(&request("zccache", "1.11.20"));
    assert_eq!(reply_code(&reply), ErrorCode::ErrorVersionBlocked);
}

#[test]
fn router_reports_missing_service_definition_as_service_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let service_root = tmp.path().join("services");
    ensure_service_definition_dir(&service_root).unwrap();
    let loader = ServiceDefinitionLoader::new(service_root);
    let registry = BackendRegistry::new();
    let router = HelloRouter::new(&loader, &registry);

    let reply = router.handle_request(&request("zccache", "1.11.20"));

    assert_eq!(reply_code(&reply), ErrorCode::ErrorServiceUnknown);
}

#[test]
fn router_reports_registry_miss_as_spawn_failed_placeholder() {
    let definition = service_definition(BrokerIsolation::SharedBroker);
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let registry = BackendRegistry::new();
    let router = HelloRouter::new(&loader, &registry);

    let reply = router.handle_request(&request("zccache", "1.11.20"));

    assert_eq!(reply_code(&reply), ErrorCode::ErrorBackendSpawnFailed);
}

#[test]
fn router_consumes_spawn_budget_on_registry_miss() {
    let definition = service_definition(BrokerIsolation::SharedBroker);
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let registry = BackendRegistry::new();
    let spawn_coordinator = Mutex::new(SpawnCoordinator::with_config(SpawnBudgetConfig::new(
        1,
        Duration::from_secs(10),
    )));
    let router = HelloRouter::new(&loader, &registry).with_spawn_coordinator(&spawn_coordinator);

    let first = router.handle_request(&request("zccache", "1.11.20"));
    let second = router.handle_request(&request("zccache", "1.11.20"));

    assert_eq!(reply_code(&first), ErrorCode::ErrorBackendSpawnFailed);
    assert_eq!(reply_code(&second), ErrorCode::ErrorRateLimited);
}

#[test]
fn router_spawn_budget_is_per_service_version() {
    let mut definition = service_definition(BrokerIsolation::SharedBroker);
    definition.version_allow_list = vec!["1.11.20".into(), "1.11.21".into()];
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let registry = BackendRegistry::new();
    let spawn_coordinator = Mutex::new(SpawnCoordinator::with_config(SpawnBudgetConfig::new(
        1,
        Duration::from_secs(10),
    )));
    let router = HelloRouter::new(&loader, &registry).with_spawn_coordinator(&spawn_coordinator);

    let first = router.handle_request(&request("zccache", "1.11.20"));
    let second_version = router.handle_request(&request("zccache", "1.11.21"));

    assert_eq!(reply_code(&first), ErrorCode::ErrorBackendSpawnFailed);
    assert_eq!(
        reply_code(&second_version),
        ErrorCode::ErrorBackendSpawnFailed
    );
}

#[test]
fn router_respects_service_definition_instance_isolation() {
    let definition = service_definition(BrokerIsolation::PrivateBroker);
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let (registry, _) = registry_with_backend(BrokerInstanceKey::Shared);
    let router = HelloRouter::new(&loader, &registry);

    let reply = router.handle_request(&request("zccache", "1.11.20"));

    assert_eq!(reply_code(&reply), ErrorCode::ErrorBackendSpawnFailed);
}
