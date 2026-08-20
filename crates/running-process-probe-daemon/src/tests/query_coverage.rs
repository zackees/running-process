use super::*;

fn query() -> wire::ProcessQuery {
    wire::ProcessQuery {
        limit: 1,
        ..Default::default()
    }
}

#[test]
fn validation_rejects_every_bounded_input_failure() {
    assert_eq!(
        ProcessQuery::from_proto(wire::ProcessQuery::default()).unwrap_err(),
        QueryError::MissingLimit
    );

    let mut value = query();
    value.limit = MAX_QUERY_LIMIT + 1;
    assert!(matches!(
        ProcessQuery::from_proto(value),
        Err(QueryError::LimitTooLarge { .. })
    ));

    for field in ["name", "exe", "cwd"] {
        let mut value = query();
        match field {
            "name" => {
                value.name_glob = "*".into();
                value.name_regex = ".*".into();
            }
            "exe" => {
                value.exe_glob = "*".into();
                value.exe_regex = ".*".into();
            }
            _ => {
                value.cwd_glob = "*".into();
                value.cwd_regex = ".*".into();
            }
        }
        assert_eq!(
            ProcessQuery::from_proto(value).unwrap_err(),
            QueryError::ConflictingSelector { field }
        );
    }

    let mut value = query();
    value.name_glob = "[".into();
    assert!(matches!(
        ProcessQuery::from_proto(value),
        Err(QueryError::InvalidSelector { field: "name", .. })
    ));
    let mut value = query();
    value.exe_regex = "(".into();
    assert!(matches!(
        ProcessQuery::from_proto(value),
        Err(QueryError::InvalidSelector { field: "exe", .. })
    ));
    let mut value = query();
    value.app_class = "x".repeat(MAX_SELECTOR_BYTES + 1);
    assert!(matches!(
        ProcessQuery::from_proto(value),
        Err(QueryError::SelectorTooLong {
            field: "app_class",
            ..
        })
    ));

    let mut value = query();
    value.pid = Some(u64::from(u32::MAX) + 1);
    assert_eq!(
        ProcessQuery::from_proto(value).unwrap_err(),
        QueryError::InvalidRange { field: "pid" }
    );
    let mut value = query();
    value.pid_min = Some(9);
    value.pid_max = Some(8);
    assert_eq!(
        ProcessQuery::from_proto(value).unwrap_err(),
        QueryError::InvalidRange { field: "pid" }
    );
    let mut value = query();
    value.start_time_min_unix_ms = Some(9);
    value.start_time_max_unix_ms = Some(8);
    assert_eq!(
        ProcessQuery::from_proto(value).unwrap_err(),
        QueryError::InvalidRange {
            field: "start_time"
        }
    );

    let mut value = query();
    value.env = vec![wire::EnvMatch::default(); MAX_ENV_MATCHES + 1];
    assert!(matches!(
        ProcessQuery::from_proto(value),
        Err(QueryError::TooManyEnvMatches { .. })
    ));
}

#[test]
fn environment_predicates_cover_presence_and_each_match_kind() {
    assert_eq!(
        compile_env(wire::EnvMatch::default()).unwrap_err(),
        QueryError::InvalidEnvKey
    );

    let too_long = wire::EnvMatch {
        key: "x".repeat(MAX_ENV_KEY_BYTES + 1),
        ..Default::default()
    };
    assert_eq!(
        compile_env(too_long).unwrap_err(),
        QueryError::InvalidEnvKey
    );

    let conflicting = wire::EnvMatch {
        key: "MODE".into(),
        value_exact: Some("test".into()),
        value_glob: "t*".into(),
        ..Default::default()
    };
    assert_eq!(
        compile_env(conflicting).unwrap_err(),
        QueryError::ConflictingSelector { field: "env" }
    );

    for env in [
        wire::EnvMatch {
            key: "MODE".into(),
            value_exact: Some("test".into()),
            ..Default::default()
        },
        wire::EnvMatch {
            key: "MODE".into(),
            value_glob: "t*".into(),
            ..Default::default()
        },
        wire::EnvMatch {
            key: "MODE".into(),
            value_regex: "^test$".into(),
            ..Default::default()
        },
        wire::EnvMatch {
            key: "MODE".into(),
            ..Default::default()
        },
    ] {
        assert_eq!(compile_env(env).unwrap().key, "MODE");
    }
}

#[test]
fn numeric_ranges_cover_open_bounds_overflow_and_inversion() {
    assert_eq!(numeric_range_u32(None, None, "pid").unwrap(), None);
    assert_eq!(
        numeric_range_u32(Some(2), None, "pid").unwrap(),
        Some((2, u32::MAX))
    );
    assert_eq!(
        numeric_range_u32(None, Some(4), "pid").unwrap(),
        Some((0, 4))
    );
    assert!(numeric_range_u32(Some(u64::from(u32::MAX) + 1), None, "pid").is_err());
    assert!(numeric_range_u32(None, Some(u64::from(u32::MAX) + 1), "pid").is_err());
    assert!(numeric_range_u32(Some(5), Some(4), "pid").is_err());

    assert_eq!(numeric_range_u64(None, None, "time").unwrap(), None);
    assert_eq!(
        numeric_range_u64(Some(2), None, "time").unwrap(),
        Some((2, u64::MAX))
    );
    assert_eq!(
        numeric_range_u64(None, Some(4), "time").unwrap(),
        Some((0, 4))
    );
    assert!(numeric_range_u64(Some(5), Some(4), "time").is_err());
}

#[test]
fn real_provider_and_engine_debug_surface_are_live() {
    let rows = SysinfoProvider.enumerate();
    assert!(rows.iter().any(|row| row.pid == std::process::id()));
    let engine = QueryEngine::default();
    let debug = format!("{engine:?}");
    assert!(debug.contains("QueryEngine"));
    assert!(debug.contains("ttl"));
}
