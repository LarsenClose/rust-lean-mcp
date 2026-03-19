use criterion::{black_box, criterion_group, criterion_main, Criterion};
use lean_lsp_client::jsonrpc::{
    Message, Notification, Request, RequestId, Response, ResponseError,
};
use lean_lsp_client::transport::{read_message, write_message};
use serde_json::json;

// ---------------------------------------------------------------------------
// JSON-RPC serialization
// ---------------------------------------------------------------------------

fn bench_request_serialize(c: &mut Criterion) {
    let req = Request::new(
        1,
        "textDocument/hover",
        Some(json!({
            "textDocument": {"uri": "file:///tmp/Test.lean"},
            "position": {"line": 10, "character": 5}
        })),
    );
    c.bench_function("jsonrpc_request_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&req)).unwrap())
    });
}

fn bench_request_deserialize(c: &mut Criterion) {
    let json_str = r#"{"jsonrpc":"2.0","id":1,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///tmp/Test.lean"},"position":{"line":10,"character":5}}}"#;
    c.bench_function("jsonrpc_request_deserialize", |b| {
        b.iter(|| serde_json::from_str::<Request>(black_box(json_str)).unwrap())
    });
}

fn bench_response_serialize(c: &mut Criterion) {
    let resp = Response {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::Number(1)),
        result: Some(json!({
            "contents": {"kind": "markdown", "value": "```lean\nNat.add : Nat → Nat → Nat\n```"}
        })),
        error: None,
    };
    c.bench_function("jsonrpc_response_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&resp)).unwrap())
    });
}

fn bench_response_deserialize(c: &mut Criterion) {
    let json_str = r#"{"jsonrpc":"2.0","id":1,"result":{"contents":{"kind":"markdown","value":"```lean\nNat.add : Nat → Nat → Nat\n```"}}}"#;
    c.bench_function("jsonrpc_response_deserialize", |b| {
        b.iter(|| serde_json::from_str::<Response>(black_box(json_str)).unwrap())
    });
}

fn bench_response_error_serialize(c: &mut Criterion) {
    let resp = Response {
        jsonrpc: "2.0".to_string(),
        id: Some(RequestId::Number(1)),
        result: None,
        error: Some(ResponseError {
            code: -32601,
            message: "Method not found".to_string(),
            data: Some(json!({"method": "unknown/method"})),
        }),
    };
    c.bench_function("jsonrpc_response_error_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&resp)).unwrap())
    });
}

fn bench_notification_serialize(c: &mut Criterion) {
    let notif = Notification::new(
        "textDocument/didOpen",
        Some(json!({
            "textDocument": {
                "uri": "file:///tmp/Test.lean",
                "languageId": "lean4",
                "version": 1,
                "text": "import Mathlib.Tactic\n\ntheorem foo : 1 + 1 = 2 := by omega"
            }
        })),
    );
    c.bench_function("jsonrpc_notification_serialize", |b| {
        b.iter(|| serde_json::to_string(black_box(&notif)).unwrap())
    });
}

fn bench_notification_deserialize(c: &mut Criterion) {
    let json_str = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///tmp/Test.lean","languageId":"lean4","version":1,"text":"import Mathlib.Tactic\n\ntheorem foo : 1 + 1 = 2 := by omega"}}}"#;
    c.bench_function("jsonrpc_notification_deserialize", |b| {
        b.iter(|| serde_json::from_str::<Notification>(black_box(json_str)).unwrap())
    });
}

fn bench_message_from_value_request(c: &mut Criterion) {
    let value = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
    c.bench_function("jsonrpc_message_from_value_request", |b| {
        b.iter(|| Message::from_value(black_box(value.clone())).unwrap())
    });
}

fn bench_message_from_value_response(c: &mut Criterion) {
    let value = json!({"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}});
    c.bench_function("jsonrpc_message_from_value_response", |b| {
        b.iter(|| Message::from_value(black_box(value.clone())).unwrap())
    });
}

fn bench_message_from_value_notification(c: &mut Criterion) {
    let value = json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///tmp/Test.lean","diagnostics":[]}});
    c.bench_function("jsonrpc_message_from_value_notification", |b| {
        b.iter(|| Message::from_value(black_box(value.clone())).unwrap())
    });
}

// ---------------------------------------------------------------------------
// Content-Length framing
// ---------------------------------------------------------------------------

fn bench_write_message(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let msg = json!({"jsonrpc":"2.0","id":1,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///tmp/Test.lean"},"position":{"line":10,"character":5}}});

    c.bench_function("transport_write_message", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut buf = Vec::with_capacity(512);
                write_message(&mut buf, black_box(&msg)).await.unwrap();
                buf
            })
        })
    });
}

fn bench_read_message(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let msg = json!({"jsonrpc":"2.0","id":1,"result":{"capabilities":{"hoverProvider":true}}});
    let body = serde_json::to_string(&msg).unwrap();
    let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    let frame_bytes = frame.into_bytes();

    c.bench_function("transport_read_message", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut reader = tokio::io::BufReader::new(black_box(frame_bytes.as_slice()));
                read_message(&mut reader).await.unwrap()
            })
        })
    });
}

fn bench_write_read_roundtrip(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let msg = json!({"jsonrpc":"2.0","id":42,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///tmp/Test.lean"},"position":{"line":5,"character":10}}});

    c.bench_function("transport_write_read_roundtrip", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut buf = Vec::with_capacity(512);
                write_message(&mut buf, black_box(&msg)).await.unwrap();
                let mut reader = tokio::io::BufReader::new(buf.as_slice());
                read_message(&mut reader).await.unwrap()
            })
        })
    });
}

fn bench_write_message_large(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Simulate a large didOpen notification with substantial file content.
    let large_text = "import Mathlib.Tactic\n".repeat(200);
    let msg = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": "file:///tmp/LargeFile.lean",
                "languageId": "lean4",
                "version": 1,
                "text": large_text
            }
        }
    });

    c.bench_function("transport_write_message_large", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut buf = Vec::with_capacity(8192);
                write_message(&mut buf, black_box(&msg)).await.unwrap();
                buf
            })
        })
    });
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

criterion_group!(
    jsonrpc_benches,
    bench_request_serialize,
    bench_request_deserialize,
    bench_response_serialize,
    bench_response_deserialize,
    bench_response_error_serialize,
    bench_notification_serialize,
    bench_notification_deserialize,
    bench_message_from_value_request,
    bench_message_from_value_response,
    bench_message_from_value_notification,
);

criterion_group!(
    transport_benches,
    bench_write_message,
    bench_read_message,
    bench_write_read_roundtrip,
    bench_write_message_large,
);

criterion_main!(jsonrpc_benches, transport_benches);
