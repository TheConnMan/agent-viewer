use agent_viewer_core::claude::rename_reply_accepted;
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

// The Fleet View rendezvous socket authenticates its first frame as `attacher-caps` and
// rejects our `rename_session` frame. The classifier must treat every rejection variant (and
// an empty/garbage read) as NOT accepted, so the caller falls back to the viewer-local name
// override instead of falsely reporting a native rename success.
#[test]
fn claude_rename_reply_rejection_is_not_accepted() {
    // The exact frame the live daemon (claude 2.1.207) sends back to our rename attempt.
    assert!(!rename_reply_accepted(br#"{"type":"reply-rejected"}"#));
    assert!(!rename_reply_accepted(br#"{"type":"auth-rejected"}"#));
    assert!(!rename_reply_accepted(br#"{"type":"rv-reply-rejected"}"#));
    // A trailing newline (the daemon line-delimits frames) must not change the verdict.
    assert!(!rename_reply_accepted(b"{\"type\":\"reply-rejected\"}\n"));
}

#[test]
fn claude_rename_reply_missing_or_garbage_is_not_accepted() {
    // No reply within the read timeout -> cannot confirm -> fall back to the override.
    assert!(!rename_reply_accepted(b""));
    // Non-JSON bytes, and JSON without a `type` field, are both unconfirmable.
    assert!(!rename_reply_accepted(b"not json at all"));
    assert!(!rename_reply_accepted(br#"{"foo":1}"#));
}

#[test]
fn claude_rename_reply_affirmative_is_accepted() {
    // A future/older daemon that acknowledges the rename with a non-rejection typed frame is
    // honored as a native success (so the override is cleared and the native name shows).
    assert!(rename_reply_accepted(br#"{"type":"reply","text":"ok"}"#));
}
