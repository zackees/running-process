//! Generated protobuf types used by the optional running-process broker client.
//!
//! This is an implementation-detail package.  Consumers should use the
//! client-gated compatibility paths re-exported by `running-process` rather
//! than depending on this crate directly.

/// Generated daemon control protocol types.
#[allow(missing_docs)]
pub mod daemon {
    include!(concat!(env!("OUT_DIR"), "/running_process.daemon.v1.rs"));
}

/// Generated broker protocol types, grouped by frozen wire version.
pub mod broker {
    /// Generated v1 broker protocol types.
    #[allow(missing_docs)]
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/running_process.broker.v1.rs"));
    }

    /// Generated v2 broker protocol types.
    #[allow(missing_docs)]
    pub mod v2 {
        include!(concat!(env!("OUT_DIR"), "/running_process.broker.v2.rs"));
    }
}

/// Errors from the broker v1 [`broker::v1::Endpoint`] smart constructors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EndpointNameError {
    /// The endpoint name or path was empty.
    #[error("endpoint name must not be empty")]
    Empty,
    /// A Windows pipe name carried the `\\\\.\\pipe\\` prefix; endpoint
    /// paths must be bare because running-process adds the prefix while
    /// resolving the endpoint.
    #[error(
        "windows pipe name must be bare (no \\\\.\\pipe\\ prefix), got {got:?}: \\
         running-process prepends the prefix when resolving the endpoint"
    )]
    PrefixedPipeName {
        /// The rejected, already-prefixed name.
        got: String,
    },
}

impl broker::v1::Frame {
    /// Build a v1 request frame with the frozen envelope defaults.
    pub fn request(payload_protocol: u32, payload: Vec<u8>) -> Self {
        Self {
            envelope_version: 1,
            kind: broker::v1::FrameKind::Request as i32,
            payload_protocol,
            payload,
            request_id: 0,
            payload_encoding: broker::v1::PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: String::new(),
            tracestate: String::new(),
        }
    }

    /// Build the v1 response frame for `request`.
    pub fn response_to(request: &Self, payload: Vec<u8>) -> Self {
        Self {
            envelope_version: 1,
            kind: broker::v1::FrameKind::Response as i32,
            payload_protocol: request.payload_protocol,
            payload,
            request_id: request.request_id,
            payload_encoding: broker::v1::PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: request.traceparent.clone(),
            tracestate: request.tracestate.clone(),
        }
    }

    /// Set the correlation request id.
    #[must_use]
    pub fn with_request_id(mut self, request_id: u64) -> Self {
        self.request_id = request_id;
        self
    }
}

impl broker::v1::Endpoint {
    /// Build a Windows named-pipe endpoint from a bare pipe name.
    pub fn windows_pipe(
        namespace_id: impl Into<String>,
        pipe_name: impl Into<String>,
    ) -> Result<Self, EndpointNameError> {
        let pipe_name = pipe_name.into();
        if pipe_name.is_empty() {
            return Err(EndpointNameError::Empty);
        }
        let lowered = pipe_name.to_ascii_lowercase().replace('/', "\\");
        if lowered.starts_with("\\\\.\\pipe\\") {
            return Err(EndpointNameError::PrefixedPipeName { got: pipe_name });
        }
        Ok(Self {
            namespace_id: namespace_id.into(),
            path: pipe_name,
        })
    }

    /// Build a Unix-domain-socket endpoint from a filesystem path.
    pub fn unix_socket(
        namespace_id: impl Into<String>,
        socket_path: impl Into<String>,
    ) -> Result<Self, EndpointNameError> {
        let socket_path = socket_path.into();
        if socket_path.is_empty() {
            return Err(EndpointNameError::Empty);
        }
        Ok(Self {
            namespace_id: namespace_id.into(),
            path: socket_path,
        })
    }
}

impl broker::v2::SessionStart {
    /// Build a contained SESSION request from the caller's current process
    /// context. Only Unicode environment entries are representable by the
    /// protobuf string vocabulary.
    pub fn from_current_process(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<String>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
            env: std::env::vars()
                .map(|(key, value)| broker::v2::SessionEnvVar { key, value })
                .collect(),
            clear_inherited_env: true,
            environment_policy: 3,
        }
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::broker::v1::{Endpoint, Frame, FrameKind, PayloadEncoding};
    use super::EndpointNameError;

    #[test]
    fn frame_constructors_keep_the_frozen_v1_defaults() {
        let mut request = Frame::request(0x7A63, b"ping".to_vec()).with_request_id(42);
        assert_eq!(request.envelope_version, 1);
        assert_eq!(request.kind, FrameKind::Request as i32);
        assert_eq!(request.payload_encoding, PayloadEncoding::None as i32);
        assert_eq!(request.request_id, 42);

        request.traceparent = "00-abc-def-01".to_owned();
        request.tracestate = "vendor=1".to_owned();
        let response = Frame::response_to(&request, b"pong".to_vec());
        assert_eq!(response.kind, FrameKind::Response as i32);
        assert_eq!(response.payload_protocol, request.payload_protocol);
        assert_eq!(response.request_id, request.request_id);
        assert_eq!(response.traceparent, request.traceparent);
        assert_eq!(response.tracestate, request.tracestate);
    }

    #[test]
    fn endpoint_constructors_keep_the_public_validation_contract() {
        let pipe = Endpoint::windows_pipe("svc", "svc-pipe").expect("bare pipe name");
        assert_eq!(pipe.namespace_id, "svc");
        assert_eq!(pipe.path, "svc-pipe");
        assert_eq!(
            Endpoint::windows_pipe("svc", r"\\.\pipe\svc-pipe"),
            Err(EndpointNameError::PrefixedPipeName {
                got: r"\\.\pipe\svc-pipe".to_owned(),
            })
        );
        assert_eq!(
            Endpoint::windows_pipe("svc", ""),
            Err(EndpointNameError::Empty)
        );
        assert_eq!(
            Endpoint::unix_socket("svc", ""),
            Err(EndpointNameError::Empty)
        );
    }
}
