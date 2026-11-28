//! A script plugin as Lightair runs it: every call checked against the permissions it holds, its
//! time budget, its storage, the hosts it may fetch, the events it gets and what its steps return.
//! Each script runs in its own isolate against a fake Lightair that records what reached it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lightair_script::{
    Budget, FetchRequest, FetchResponse, Host, Needs, Package, Script, State, for_method, may_fetch,
};
use serde_json::{Value, json};

/// A Lightair that answers every call and fetch, and keeps what reached it.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    fetched: Mutex<Vec<String>>,
}

impl Host for Fake {
    fn call(&self, method: &str, _params: Value) -> Result<Value, String> {
        self.calls.lock().unwrap().push(method.to_string());
        Ok(json!({"ok": true}))
    }
    fn fetch(&self, req: &FetchRequest) -> Result<FetchResponse, String> {
        self.fetched.lock().unwrap().push(req.url.clone());
        Ok(FetchResponse { status: 200, headers: Default::default(), body: "{\"n\":1}".into() })
    }
}

fn start(code: &str, permissions: &[&str], budget: Budget, data_dir: Option<PathBuf>) -> (Script, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let package = Package {
        id: "io.github.nova.test".into(),
        name: "Test".into(),
        code: code.into(),
        permissions: permissions.iter().map(|p| p.to_string()).collect(),
        optional: vec![],
        data_dir,
        settings: json!({}),
    };
    (Script::start(package, fake.clone(), budget), fake)
}

fn calls(f: &Fake) -> Vec<String> {
    f.calls.lock().unwrap().clone()
}

#[test]
fn a_method_is_refused_without_its_permission_and_never_reaches_lightair() {
    let code = r#"
        lightair.command({ id: "read", title: "Read", run: () => lightair.call("state.get", {}) });
        lightair.command({ id: "switch", title: "Switch", run: () => lightair.call("scene.switch", { scene: "Main" }) });
    "#;
    let (s, fake) = start(code, &["project.read"], Budget::default(), None);
    assert!(s.command("read").is_ok(), "a read with project.read goes through");
    let err = s.command("switch").expect_err("changing the show needs project.write");
    assert!(err.contains("project.write"), "the refusal names the permission: {err}");
    assert_eq!(calls(&fake), vec!["state.get".to_string()], "only the allowed call reached Lightair");
    assert!(
        s.records().iter().any(|r| !r.allowed && r.what == "scene.switch"),
        "the refused call is recorded for the activity panel: {:?}",
        s.records()
    );
}

#[test]
fn methods_closed_to_scripts_are_refused_whatever_they_hold() {
    let code = r#"lightair.command({ id: "quit", title: "Quit", run: () => lightair.call("app.exit", {}) });"#;
    let all = ["project.read", "project.write", "outputs.control", "chat.read", "chat.write"];
    let (s, fake) = start(code, &all, Budget::default(), None);
    assert!(s.command("quit").is_err());
    assert!(calls(&fake).is_empty(), "app.exit never reached Lightair: {:?}", calls(&fake));
}

#[test]
fn each_kind_of_method_asks_for_the_permission_the_script_api_gives_it() {
    for (method, needs) in [
        ("scene.list", "project.read"),
        ("state.get", "project.read"),
        ("scene.switch", "project.write"),
        ("source.create", "project.write"),
        ("output.start", "outputs.control"),
        ("record.stop", "outputs.control"),
        ("output.list", "project.read"),
        ("chat.send", "chat.write"),
        ("chat.history", "chat.read"),
        ("preview.frame", "screen.capture"),
    ] {
        assert_eq!(for_method(method), Needs::Permission(needs), "{method}");
    }
    for closed in ["app.exit", "auth.login", "pair.start", "plugins.list", "script.run", "events.subscribe"] {
        assert!(matches!(for_method(closed), Needs::Refused(_)), "{closed} is never open to scripts");
    }
}

#[test]
fn a_callback_over_its_budget_is_stopped_and_running_over_again_suspends_the_script() {
    let code = r#"lightair.command({ id: "spin", title: "Spin", run: () => { for (;;) {} } });
                  lightair.command({ id: "ok", title: "Ok", run: () => 1 });"#;
    let budget = Budget { per_call: Duration::from_millis(30), overruns: 2, ..Budget::default() };
    let (s, _) = start(code, &[], budget, None);
    let first = s.command("spin").expect_err("an endless loop is stopped");
    assert!(first.contains("too long"), "{first}");
    assert_eq!(s.state(), State::Running, "one overrun only stops that callback");
    assert!(s.command("spin").is_err());
    assert!(matches!(s.state(), State::Suspended(_)), "a second overrun within a minute suspends it: {:?}", s.state());
    assert!(s.command("ok").is_err(), "a suspended script runs nothing more");
}

#[test]
fn storage_is_kept_for_the_package_and_a_write_past_its_quota_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let code = r#"
        lightair.command({ id: "small", title: "Small", run: () => lightair.storage.set("count", 3) });
        lightair.command({ id: "big", title: "Big", run: () => lightair.storage.set("count", "x".repeat(400)) });
        lightair.command({ id: "get", title: "Get", run: () => lightair.storage.get("count") });
    "#;
    let budget = Budget { storage: 256, ..Budget::default() };
    {
        let (s, _) = start(code, &[], budget.clone(), Some(dir.path().to_path_buf()));
        s.command("small").unwrap();
        let err = s.command("big").expect_err("400 bytes don't fit in 256");
        assert!(err.contains("storage is full"), "{err}");
        assert_eq!(s.command("get").unwrap(), json!(3), "the refused write left the value as it was");
    }
    let (again, _) = start(code, &[], budget, Some(dir.path().to_path_buf()));
    assert_eq!(again.command("get").unwrap(), json!(3), "storage outlives the script that wrote it");
}

#[test]
fn a_script_fetches_only_hosts_it_declared() {
    let code = r#"
        lightair.command({ id: "ok", title: "Ok", run: () => lightair.fetch("https://api.example.com/v1/x").status });
        lightair.command({ id: "other", title: "Other", run: () => lightair.fetch("https://collect.example.org/x").status });
    "#;
    let (s, fake) = start(code, &["network:api.example.com"], Budget::default(), None);
    assert_eq!(s.command("ok").unwrap(), json!(200));
    let err = s.command("other").expect_err("an undeclared host is refused");
    assert!(err.contains("collect.example.org"), "{err}");
    assert_eq!(
        *fake.fetched.lock().unwrap(),
        vec!["https://api.example.com/v1/x".to_string()],
        "only the declared host was fetched"
    );
}

#[test]
fn fetch_rules_cover_wildcards_local_ports_and_other_schemes() {
    let p = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(may_fetch(&p(&["network:*.example.com"]), "https://a.b.example.com/x").is_ok());
    assert!(may_fetch(&p(&["network:*.example.com"]), "https://example.com.evil.net/").is_err());
    assert!(may_fetch(&p(&["network:localhost:8001"]), "http://localhost:8001/api").is_ok());
    assert!(may_fetch(&p(&["network:localhost:8001"]), "http://localhost:9000/api").is_err(), "only the declared port");
    assert!(may_fetch(&p(&["network:*"]), "http://127.0.0.1:8001/").is_err(), "any host doesn't mean this computer");
    assert!(may_fetch(&p(&["network:*"]), "file:///etc/passwd").is_err());
}

#[test]
fn events_reach_a_script_that_holds_their_permission() {
    let code = r#"
        let seen = [];
        lightair.on("chat", (e) => seen.push(e.text));
        lightair.step({ id: "seen", title: "Seen", run: () => seen });
    "#;
    let (s, _) = start(code, &["chat.read"], Budget::default(), None);
    s.event("chat", json!({"text": "hello"}));
    s.event("chat", json!({"text": "!count"}));
    assert_eq!(s.step("seen", json!({})).unwrap(), json!(["hello", "!count"]));
}

#[test]
fn listening_to_chat_without_chat_read_stops_the_script_from_loading() {
    let (s, _) = start(r#"lightair.on("chat", () => {});"#, &[], Budget::default(), None);
    // a round trip: by its answer, loading has finished
    assert!(s.command("anything").is_err());
    match s.state() {
        State::Failed(e) => assert!(e.contains("chat.read"), "{e}"),
        other => panic!("expected it not to load, got {other:?}"),
    }
}

#[test]
fn a_steps_result_is_what_its_handler_returns_or_resolves_to() {
    let code = r#"
        lightair.step({ id: "add", title: "Add", run: (p) => p.by + 1 });
        lightair.step({ id: "later", title: "Later", run: () => Promise.resolve("done") });
    "#;
    let (s, _) = start(code, &[], Budget::default(), None);
    assert_eq!(s.step("add", json!({"by": 4})).unwrap(), json!(5));
    assert_eq!(s.step("later", json!({})).unwrap(), json!("done"));
    let steps: Vec<String> = s.declared().steps.iter().map(|d| d.id.clone()).collect();
    assert_eq!(steps, vec!["add", "later"], "both steps are offered to the automations");
}

// ---------------------------------------------------------------- optional permissions

/// A Lightair whose user grants the listed optional permissions when asked, and nothing else.
#[derive(Default)]
struct Granting {
    grants: Vec<String>,
    calls: Mutex<Vec<String>>,
    released: Mutex<Vec<String>>,
}

impl Host for Granting {
    fn call(&self, method: &str, _params: Value) -> Result<Value, String> {
        self.calls.lock().unwrap().push(method.to_string());
        Ok(json!({"ok": true}))
    }
    fn fetch(&self, _req: &FetchRequest) -> Result<FetchResponse, String> {
        Err("no network here".into())
    }
    fn request_permissions(&self, permissions: &[String], _reason: &str) -> Vec<String> {
        permissions.iter().filter(|p| self.grants.contains(p)).cloned().collect()
    }
    fn release_permissions(&self, permissions: &[String]) {
        self.released.lock().unwrap().extend(permissions.iter().cloned());
    }
}

const GO_LIVE: &str = r#"
lightair.command({ id: "go", title: "Go live", run: () => lightair.call("output.start", {}) });
lightair.command({ id: "ask", title: "Ask", run: () => lightair.permissions.request(["outputs.control"], "to go live for you") });
lightair.command({ id: "ask-chat", title: "Ask chat", run: () => lightair.permissions.request(["chat.write"], "to talk") });
lightair.command({ id: "give-back", title: "Give back", run: () => lightair.permissions.release(["outputs.control"]) });
"#;

fn start_optional(grants: &[&str]) -> (Script, Arc<Granting>) {
    let host = Arc::new(Granting { grants: grants.iter().map(|g| g.to_string()).collect(), ..Default::default() });
    let package = Package {
        id: "io.github.nova.live".into(),
        name: "Live".into(),
        code: GO_LIVE.into(),
        permissions: vec![],
        optional: vec!["outputs.control".into()],
        data_dir: None,
        settings: json!({}),
    };
    (Script::start(package, host.clone(), Budget::default()), host)
}

#[test]
fn a_method_needing_an_optional_permission_works_only_once_the_user_grants_it() {
    let (s, host) = start_optional(&["outputs.control"]);
    assert!(s.command("go").is_err(), "an optional permission isn't held before it's asked for");
    assert!(host.calls.lock().unwrap().is_empty());
    assert_eq!(s.command("ask").unwrap(), json!(["outputs.control"]));
    s.command("go").expect("granted now");
    assert_eq!(*host.calls.lock().unwrap(), ["output.start"]);
}

#[test]
fn when_the_user_says_no_the_method_stays_refused() {
    let (s, host) = start_optional(&[]);
    assert_eq!(s.command("ask").unwrap(), json!([]));
    assert!(s.command("go").is_err());
    assert!(host.calls.lock().unwrap().is_empty());
}

#[test]
fn a_script_cant_ask_for_a_permission_it_didnt_list_as_optional() {
    let (s, _host) = start_optional(&["chat.write"]);
    assert!(s.command("ask-chat").is_err());
}

#[test]
fn giving_a_permission_back_refuses_the_method_again_and_tells_lightair() {
    let (s, host) = start_optional(&["outputs.control"]);
    s.command("ask").unwrap();
    s.command("give-back").unwrap();
    assert!(s.command("go").is_err());
    assert_eq!(*host.released.lock().unwrap(), ["outputs.control"]);
}
