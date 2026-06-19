//! v2 broker protocol module.
//!
//! Houses the prost-generated types for the `running_process.broker.v2`
//! package — currently the `ServiceDefinition` envelope and the
//! `HttpServerCapability` optional sub-message introduced in #483.
//!
//! v2 runs in parallel with v1 (`super::protocol`) through the broker
//! v2 rollout; v1's types are FROZEN FOREVER (#228) so all new
//! capability fields land here instead.

#[allow(missing_docs)]
mod prost_generated {
    include!(concat!(env!("OUT_DIR"), "/running_process.broker.v2.rs"));
}

pub use prost_generated::*;

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    /// `ServiceDefinition` round-trips with no HTTP capability — the
    /// optional field is absent on both sides.
    #[test]
    fn service_definition_without_http_round_trips() {
        let original = ServiceDefinition {
            service_name: "zccache".to_owned(),
            http_server: None,
        };

        let bytes = original.encode_to_vec();
        let decoded = ServiceDefinition::decode(bytes.as_slice())
            .expect("encoded ServiceDefinition decodes");

        assert_eq!(decoded.service_name, "zccache");
        assert!(decoded.http_server.is_none());
    }

    /// `ServiceDefinition` round-trips with an `HttpServerCapability`
    /// populated — all three fields survive.
    #[test]
    fn service_definition_with_http_round_trips() {
        let original = ServiceDefinition {
            service_name: "fbuild".to_owned(),
            http_server: Some(HttpServerCapability {
                bind_addr: "127.0.0.1".to_owned(),
                health_path: "/healthz".to_owned(),
                display_name: "fbuild status".to_owned(),
            }),
        };

        let bytes = original.encode_to_vec();
        let decoded = ServiceDefinition::decode(bytes.as_slice())
            .expect("encoded ServiceDefinition decodes");

        let cap = decoded
            .http_server
            .expect("http_server survives round-trip");
        assert_eq!(decoded.service_name, "fbuild");
        assert_eq!(cap.bind_addr, "127.0.0.1");
        assert_eq!(cap.health_path, "/healthz");
        assert_eq!(cap.display_name, "fbuild status");
    }

    /// Empty `HttpServerCapability` survives a round-trip — defaults are
    /// applied by the loader/consumer, not by the proto encoder.
    #[test]
    fn http_server_capability_empty_defaults_survive_round_trip() {
        let original = ServiceDefinition {
            service_name: "minimal".to_owned(),
            http_server: Some(HttpServerCapability::default()),
        };

        let bytes = original.encode_to_vec();
        let decoded = ServiceDefinition::decode(bytes.as_slice())
            .expect("encoded ServiceDefinition decodes");

        let cap = decoded
            .http_server
            .expect("http_server survives round-trip");
        assert!(cap.bind_addr.is_empty());
        assert!(cap.health_path.is_empty());
        assert!(cap.display_name.is_empty());
    }

    /// `BackendHttpReady` carries the daemon's OS-allocated port back to
    /// the broker; encodes/decodes without loss.
    #[test]
    fn backend_http_ready_round_trips() {
        let original = BackendHttpReady { port: 49_152 };

        let bytes = original.encode_to_vec();
        let decoded =
            BackendHttpReady::decode(bytes.as_slice()).expect("BackendHttpReady decodes");

        assert_eq!(decoded.port, 49_152);
    }

    /// `GetBrokerHttpEndpointRequest` is an empty marker; encoding +
    /// decoding it produces the same default-constructed message.
    #[test]
    fn get_broker_http_endpoint_request_round_trips_empty() {
        let original = GetBrokerHttpEndpointRequest::default();

        let bytes = original.encode_to_vec();
        let decoded = GetBrokerHttpEndpointRequest::decode(bytes.as_slice())
            .expect("GetBrokerHttpEndpointRequest decodes");

        assert_eq!(decoded, GetBrokerHttpEndpointRequest::default());
    }

    /// `GetBrokerHttpEndpointResponse` round-trips both fields (port + pid).
    #[test]
    fn get_broker_http_endpoint_response_round_trips() {
        let original = GetBrokerHttpEndpointResponse {
            port: 8765,
            pid: 12_345,
        };

        let bytes = original.encode_to_vec();
        let decoded = GetBrokerHttpEndpointResponse::decode(bytes.as_slice())
            .expect("GetBrokerHttpEndpointResponse decodes");

        assert_eq!(decoded.port, 8765);
        assert_eq!(decoded.pid, 12_345);
    }
}
