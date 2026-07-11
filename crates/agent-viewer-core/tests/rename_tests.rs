use agent_viewer_core::codex::cli::name_set_request;
use serde_json::Value;

#[test]
fn codex_name_set_request_shape() {
    // A name containing a quote and a newline must survive serialization intact
    // (serde-built, never string-interpolated).
    let tricky = "name with \" and \n newline";
    let line = name_set_request(7, "019f-thread", tricky);

    // One valid JSON object per line.
    let v: Value = serde_json::from_str(line.trim()).expect("valid json-rpc line");
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["method"], "thread/name/set");
    assert_eq!(v["params"]["threadId"], "019f-thread");
    assert_eq!(v["params"]["name"], tricky);
    assert_eq!(v["id"], 7);
}
