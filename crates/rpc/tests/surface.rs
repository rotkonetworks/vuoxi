use serde_json::{Value, json};
use vuoxi_rpc::service::{AllowLayer, Layer, handler};
use vuoxi_rpc::verbs::fake_router;
use vuoxi_rpc::{ErrorCode, Router, RpcRequest, RpcResponse, SCHEMA, client, drive, server};

fn call<S>(svc: &mut S, method: &str, params: Value) -> RpcResponse
where
    S: vuoxi_rpc::Service<RpcRequest, Response = RpcResponse, Error = std::convert::Infallible>,
{
    drive(svc, RpcRequest::new(7, method, Some(params)))
}

fn result(r: RpcResponse) -> Value {
    assert!(r.error.is_none(), "unexpected error: {:?}", r.error);
    r.result.unwrap()
}

fn code(r: &RpcResponse) -> ErrorCode {
    r.error.as_ref().expect("expected an error").data.code
}

#[test]
fn router_dispatches_and_reports_unknown_methods() {
    let mut r = Router::new().route(
        "ping",
        handler(|q: RpcRequest| {
            RpcResponse::ok(
                q.id,
                &vuoxi_rpc::types::Reverted {
                    plan: "x".into(),
                    head: 1,
                },
            )
        }),
    );
    let ok = call(&mut r, "ping", json!({}));
    assert_eq!(ok.id, Some(json!(7)));
    assert_eq!(result(ok)["plan"], "x");
    let missing = call(&mut r, "nope", json!({}));
    assert_eq!(code(&missing), ErrorCode::MethodNotFound);
    assert_eq!(missing.error.unwrap().code, -32601);
}

#[test]
fn every_result_carries_kind_and_schema() {
    let mut r = fake_router();
    let p = result(call(&mut r, "plan", json!({})));
    let id = p["id"].as_str().unwrap().to_string();
    let cases = [
        ("status", json!({}), "status"),
        ("changes", json!({}), "changes"),
        ("conflicts", json!({}), "conflicts"),
        ("plan", json!({"folder": "docs"}), "plan"),
        ("apply", json!({"plan": id}), "applied"),
        ("revert", json!({"plan": id}), "reverted"),
    ];
    for (method, params, kind) in cases {
        let v = result(call(&mut r, method, params));
        assert_eq!(v["kind"], kind, "{method}");
        assert_eq!(v["schema"], SCHEMA, "{method}");
    }
}

#[test]
fn errors_carry_stable_codes() {
    let mut r = fake_router();
    let e = call(&mut r, "apply", json!({"plan": "plan-99"}));
    assert_eq!(code(&e), ErrorCode::NotFound);
    let d = e.error.unwrap();
    assert_eq!((d.data.kind.as_str(), d.data.schema), ("error", SCHEMA));

    assert_eq!(
        code(&call(&mut r, "plan", json!({"folder": "nope"}))),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(&call(&mut r, "apply", json!({}))),
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        code(&call(&mut r, "changes", json!({"bogus": 1}))),
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        code(&call(&mut r, "changes", json!({"limit": 0}))),
        ErrorCode::InvalidRequest
    );
    assert_eq!(
        code(&call(&mut r, "changes", json!({"since": "x"}))),
        ErrorCode::InvalidRequest
    );

    let v = serde_json::to_value(code(&call(&mut r, "apply", json!({})))).unwrap();
    assert_eq!(v, "invalid_request");
}

#[test]
fn pages_are_capped_and_walk_the_journal() {
    let mut r = fake_router();
    let big = result(call(&mut r, "changes", json!({"limit": 100_000})));
    assert_eq!(
        big["items"].as_array().unwrap().len(),
        200,
        "clamped to the cap"
    );
    assert_eq!(big["next_cursor"], 200);

    let dflt = result(call(&mut r, "changes", json!({})));
    assert_eq!(dflt["items"].as_array().unwrap().len(), 50);

    let mut since = 0;
    let mut seen = 0;
    loop {
        let p = result(call(
            &mut r,
            "changes",
            json!({"since": since, "limit": 100}),
        ));
        seen += p["items"].as_array().unwrap().len();
        match p["next_cursor"].as_u64() {
            Some(c) => since = c,
            None => break,
        }
    }
    assert_eq!(seen, 250, "pages cover the journal exactly once");

    let end = result(call(&mut r, "changes", json!({"since": 250})));
    assert!(end["items"].as_array().unwrap().is_empty() && end["next_cursor"].is_null());

    let c = result(call(&mut r, "conflicts", json!({"limit": 2})));
    assert_eq!(c["items"].as_array().unwrap().len(), 2);
    assert_eq!(c["next_cursor"], 120);
}

#[test]
fn plan_apply_revert_lifecycle() {
    let mut r = fake_router();
    let head = result(call(&mut r, "status", json!({})))["head"]
        .as_u64()
        .unwrap();
    let p = result(call(&mut r, "plan", json!({})));
    assert_eq!(p["state"], "pending");
    assert_eq!(
        result(call(&mut r, "status", json!({})))["head"],
        head,
        "plan changes nothing"
    );
    let id = p["id"].as_str().unwrap();

    let a = result(call(&mut r, "apply", json!({"plan": id})));
    assert!(a["head"].as_u64().unwrap() > head);
    assert_eq!(
        code(&call(&mut r, "apply", json!({"plan": id}))),
        ErrorCode::InvalidRequest
    );

    let v = result(call(&mut r, "revert", json!({"plan": id})));
    assert_eq!(v["head"], head);
    assert_eq!(
        code(&call(&mut r, "revert", json!({"plan": id}))),
        ErrorCode::InvalidRequest
    );
}

#[test]
fn revert_is_last_in_first_out() {
    let mut r = fake_router();
    let a = result(call(&mut r, "plan", json!({})))["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = result(call(&mut r, "plan", json!({})))["id"]
        .as_str()
        .unwrap()
        .to_string();
    result(call(&mut r, "apply", json!({"plan": a})));
    result(call(&mut r, "apply", json!({"plan": b})));
    assert_eq!(
        code(&call(&mut r, "revert", json!({"plan": a}))),
        ErrorCode::InvalidRequest
    );
    result(call(&mut r, "revert", json!({"plan": b})));
    result(call(&mut r, "revert", json!({"plan": a})));
}

#[test]
fn layers_wrap_the_router() {
    let mut r = AllowLayer::read_only().layer(fake_router());
    result(call(&mut r, "status", json!({})));
    let denied = call(&mut r, "apply", json!({"plan": "plan-1"}));
    assert_eq!(code(&denied), ErrorCode::CapabilityDenied);
    assert_eq!(
        code(&call(&mut r, "plan", json!({}))),
        ErrorCode::CapabilityDenied
    );
}

#[test]
fn line_framing_handles_garbage_and_notifications() {
    let mut r = fake_router();
    let bad = server::handle_line(&mut r, "not json").unwrap();
    let v: Value = serde_json::from_str(&bad).unwrap();
    assert_eq!(v["error"]["data"]["code"], "invalid_request");
    assert!(v["id"].is_null());

    let wrong =
        server::handle_line(&mut r, r#"{"jsonrpc":"1.0","id":1,"method":"status"}"#).unwrap();
    assert!(wrong.contains("invalid_request"));

    assert!(server::handle_line(&mut r, r#"{"jsonrpc":"2.0","method":"status"}"#).is_none());
}

#[test]
fn socket_round_trip() {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("rpc-roundtrip.sock");
    let listener = server::bind(&path).unwrap();
    std::thread::spawn(move || server::serve(listener, fake_router()));

    let resp = client::call(&path, "status", None).unwrap();
    assert_eq!(resp.result.unwrap()["kind"], "status");
    let resp = client::call(&path, "changes", Some(json!({"limit": 3}))).unwrap();
    assert_eq!(resp.result.unwrap()["items"].as_array().unwrap().len(), 3);

    // A second bind refuses a live socket.
    assert!(server::bind(&path).is_err());
    let _ = std::fs::remove_file(&path);
}
