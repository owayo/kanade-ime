use kanade_ipc::{frontend::*, *};
use prost::Message;
use std::{cell::Cell, io, rc::Rc};

struct Wire {
    welcome: Welcome,
    open: Rc<Cell<bool>>,
}
impl Transport for Wire {
    fn exchange(&mut self, request: &Request) -> io::Result<Response> {
        assert!(matches!(request.body, Some(request::Body::Hello(_))));
        Ok(Response {
            body: Some(response::Body::Welcome(self.welcome.clone())),
        })
    }
    fn notify(&mut self, _: &Request) -> io::Result<()> {
        unreachable!("a policy query does not send keys or acknowledgements")
    }
    fn is_open(&mut self) -> bool {
        self.open.get()
    }
}
fn wire(policy: Option<LearningPolicy>, legacy: Vec<String>) -> Wire {
    Wire {
        welcome: Welcome {
            version: CURRENT_VERSION,
            connection: 1,
            learning_apps: legacy,
            learning_policy: policy,
        },
        open: Rc::new(Cell::new(true)),
    }
}
fn policy(enabled: bool, excluded: &[&str]) -> Option<LearningPolicy> {
    Some(LearningPolicy {
        enabled,
        excluded_apps: excluded.iter().map(|v| (*v).into()).collect(),
    })
}
#[test]
fn all_known_apps_are_normal_except_exclusions_and_unknown_identities() {
    let client = Client::connect(wire(policy(true, &["com.example.Secret"]), vec![])).unwrap();
    for app in [
        "com.example.First",
        "com.example.Second",
        "com.apple.TextEdit",
    ] {
        assert_eq!(client.privacy_for_app(app), Privacy::Normal);
    }
    for app in [
        "com.example.Secret",
        "",
        " ",
        "com.example.Editor\n",
        " editor",
    ] {
        assert_eq!(client.privacy_for_app(app), Privacy::Unknown);
    }
    assert_eq!(client.privacy_for_app(&"x".repeat(257)), Privacy::Unknown);
    let disabled =
        Client::connect(wire(policy(false, &[]), vec!["com.example.First".into()])).unwrap();
    assert_eq!(
        disabled.privacy_for_app("com.example.First"),
        Privacy::Unknown
    );
}
#[test]
fn older_engine_scope_never_becomes_all_apps() {
    let client = Client::connect(wire(None, vec!["com.apple.TextEdit".into()])).unwrap();
    assert_eq!(
        client.privacy_for_app("com.apple.TextEdit"),
        Privacy::Normal
    );
    assert_eq!(
        client.privacy_for_app("com.example.Editor"),
        Privacy::Unknown
    );
    let client = Client::connect(wire(None, vec![])).unwrap();
    assert_eq!(
        client.privacy_for_app("com.apple.TextEdit"),
        Privacy::Unknown
    );
}
#[test]
fn reconnect_adopts_new_exclusions_without_changing_legacy_scope() {
    let first = wire(policy(true, &[]), vec![]);
    let open = first.open.clone();
    let mut client = Client::connect(first).unwrap();
    assert_eq!(
        client.privacy_for_app("com.example.Secret"),
        Privacy::Normal
    );
    open.set(false);
    assert!(client.needs_transport());
    client
        .reattach(wire(policy(true, &["com.example.Secret"]), vec![]))
        .unwrap();
    assert_eq!(
        client.privacy_for_app("com.example.Secret"),
        Privacy::Unknown
    );
    assert_eq!(
        client.privacy_for_app("com.example.Editor"),
        Privacy::Normal
    );
}
#[test]
fn old_welcome_decoders_get_an_empty_allowlist() {
    #[derive(Clone, PartialEq, Message)]
    struct OldWelcome {
        #[prost(uint32, tag = "1")]
        version: u32,
        #[prost(uint64, tag = "2")]
        connection: u64,
        #[prost(string, repeated, tag = "3")]
        learning_apps: Vec<String>,
    }
    let bytes = wire(policy(true, &["com.example.Secret"]), vec![])
        .welcome
        .encode_to_vec();
    let old = OldWelcome::decode(bytes.as_slice()).unwrap();
    assert_eq!(old.connection, 1);
    assert!(old.learning_apps.is_empty());
}
