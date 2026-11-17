//! A trusted, statically linked native fixture proves the contract without a
//! Wasmtime dependency. This is not a native library loader or a sandbox.

use std::cell::Cell;
use std::rc::Rc;

use netfyr_state::{Match, Source, Value};
use serde_json::json;

use crate::*;

fn policy() -> Policy {
    Policy {
        schemas: [(
            "test_device".into(),
            r#"{"type":"object","required":["uuid"],"properties":{"uuid":{"type":"string"}}}"#
                .into(),
        )]
        .into(),
        ..Policy::default()
    }
}

fn config_schema() -> Json {
    json!({"type":"object","properties":{"uuid":{"type":"string"},"fail":{"type":"boolean"}}})
}

fn observation(fields: &str) -> Observation {
    Observation {
        devices: vec![Device {
            name: "br0".into(),
            device_type: "test_device".into(),
            fields_json: fields.into(),
        }],
        ..Observation::default()
    }
}

struct NativeFixture {
    calls: Rc<Cell<usize>>,
    grants: Vec<String>,
}

impl ObserverBackend for NativeFixture {
    fn observe(&mut self, config: &str) -> Result<Observation, Error> {
        self.calls.set(self.calls.get() + 1);
        let config: Json = serde_json::from_str(config).unwrap();
        if config.get("fail").and_then(Json::as_bool) == Some(true) {
            return Err(Error::new(
                ErrorKind::Plugin(FailureKind::Unreachable),
                "native fixture unavailable",
            ));
        }
        let uuid = config
            .get("uuid")
            .and_then(Json::as_str)
            .unwrap_or_else(|| self.grants.first().map(String::as_str).unwrap_or("0123"));
        let mut output = observation(&json!({"test_device":{"uuid":uuid}}).to_string());
        output.diagnostics.push(Diagnostic {
            device_name: Some("br0".into()),
            field: Some("uuid".into()),
            message: "fixture diagnostic".into(),
        });
        output.logs.push(LogRecord {
            level: Level::Debug,
            message: "fixture log".into(),
        });
        Ok(output)
    }
}

fn factory(calls: Rc<Cell<usize>>) -> impl ObserverFactory {
    move |mut supplied: Policy| -> Result<Box<dyn Observer>, Error> {
        supplied.schemas = policy().schemas;
        let backend = NativeFixture {
            calls: Rc::clone(&calls),
            grants: supplied.socket_paths.clone(),
        };
        Ok(Box::new(ValidatedObserver::new(
            backend,
            supplied,
            Some(&config_schema()),
        )?))
    }
}

#[test]
fn native_fixture_runs_through_the_object_safe_registry() {
    let calls = Rc::new(Cell::new(0));
    let mut registry = ObserverRegistry::default();
    registry
        .register("fixture".into(), factory(Rc::clone(&calls)))
        .unwrap();
    let mut observer: Box<dyn Observer> = registry.create("fixture", Policy::default()).unwrap();
    let snapshot = observer
        .query(r#"{"uuid":"0123"}"#, &Match::default())
        .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(
        snapshot.states[0].fields["name"],
        Value::String("br0".into())
    );
    assert!(matches!(snapshot.states[0].source, Source::Kernel));
    assert_eq!(
        snapshot.states[0].match_spec.r#type.as_deref(),
        Some("test_device")
    );
    assert!(observer.schemas().validate(&snapshot.states[0]).is_empty());
    assert!(
        !observer
            .schemas()
            .validate_writable(&snapshot.states[0])
            .is_empty()
    );
    assert!(
        observer
            .schemas()
            .to_yaml_query(&snapshot.states)
            .unwrap()
            .contains("0123")
    );
    assert_eq!(snapshot.diagnostics[0].message, "fixture diagnostic");
    assert_eq!(snapshot.logs[0].level, Level::Debug);
}

#[test]
fn config_is_checked_before_backend_execution_and_selectors_are_shared() {
    let calls = Rc::new(Cell::new(0));
    let mut registry = ObserverRegistry::default();
    registry
        .register("fixture".into(), factory(Rc::clone(&calls)))
        .unwrap();
    let mut observer = registry.create("fixture", Policy::default()).unwrap();
    for config in ["not JSON", "[]", r#"{"uuid":12}"#, r#"{"unknown":true}"#] {
        assert_eq!(
            observer.query(config, &Match::default()).unwrap_err().kind,
            ErrorKind::InvalidConfig
        );
    }
    assert_eq!(calls.get(), 0);
    let selector = Match {
        name: Some("other".into()),
        ..Match::default()
    };
    assert!(observer.query("{}", &selector).unwrap().states.is_empty());
    assert_eq!(calls.get(), 1);
    assert_eq!(
        observer
            .query(r#"{"fail":true}"#, &Match::default())
            .unwrap_err()
            .kind,
        ErrorKind::Plugin(FailureKind::Unreachable)
    );
}

#[test]
fn schema_enforcement_rejects_bad_native_or_adapter_output_before_filtering() {
    let contract = Contract::new(policy(), None).unwrap();
    for fields in [
        "[]",
        r#"{"test_device":{"uuid":12}}"#,
        r#"{"test_device":{"uuid":null}}"#,
        r#"{"test_device":{"uuid":"id","extra":true}}"#,
        r#"{"test_device":{}}"#,
        r#"{"test_device":{},"mtu":1500}"#,
    ] {
        let selector = Match {
            name: Some("not-br0".into()),
            ..Match::default()
        };
        assert_eq!(
            contract
                .validate(observation(fields), &selector)
                .unwrap_err()
                .kind,
            ErrorKind::InvalidOutput
        );
    }
    let mut duplicate = observation(r#"{"test_device":{"uuid":"id"}}"#);
    duplicate.devices.push(duplicate.devices[0].clone());
    assert!(contract.validate(duplicate, &Match::default()).is_err());
    let mut undeclared = observation(r#"{"test_device":{"uuid":"id"}}"#);
    undeclared.devices[0].device_type = "ethernet".into();
    assert!(contract.validate(undeclared, &Match::default()).is_err());
}

#[test]
fn factories_receive_isolated_instance_policies_and_do_not_override_registrations() {
    let mut registry = ObserverRegistry::default();
    registry
        .register("fixture".into(), factory(Rc::new(Cell::new(0))))
        .unwrap();
    assert!(
        registry
            .register("fixture".into(), factory(Rc::new(Cell::new(0))))
            .is_err()
    );
    assert!(registry.create("missing", Policy::default()).is_err());
    let mut first = registry
        .create(
            "fixture",
            Policy {
                socket_paths: vec!["/first".into()],
                ..Policy::default()
            },
        )
        .unwrap();
    let mut second = registry.create("fixture", Policy::default()).unwrap();
    let first = first.query("{}", &Match::default()).unwrap();
    let second = second.query("{}", &Match::default()).unwrap();
    let field = |snapshot: &Snapshot| snapshot.states[0].fields["test_device"].to_string();
    assert!(field(&first).contains("/first"));
    assert!(!field(&second).contains("/first"));
}

#[test]
fn invalid_policy_and_output_budgets_are_execution_independent() {
    assert!(
        Contract::new(
            Policy {
                fuel: 0,
                ..policy()
            },
            None
        )
        .is_err()
    );
    assert!(
        Contract::new(
            Policy {
                timeout: Duration::ZERO,
                ..policy()
            },
            None
        )
        .is_err()
    );
    let contract = Contract::new(policy(), None).unwrap();
    let mut oversized = observation(r#"{"test_device":{"uuid":"id"}}"#);
    oversized.logs.push(LogRecord {
        level: Level::Info,
        message: "x".repeat(4097),
    });
    assert!(contract.validate(oversized, &Match::default()).is_err());
    assert_eq!(
        contract.check_config(&"x".repeat(65537)).unwrap_err().kind,
        ErrorKind::InvalidConfig
    );
}
