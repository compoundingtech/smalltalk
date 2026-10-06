use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::json;
use st3_client::{Client, RawTerminalAttachment, RawTerminalMode};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{Request, Response},
};

// tokio-tungstenite fixes the handshake callback's error type.
#[allow(clippy::result_large_err)]
#[tokio::test]
async fn selected_use_is_sequenced_out_of_band_and_server_text_becomes_eof() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("raw-terminal.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let upload = b"\0\0\0\0\x04\0\xff\r\n";
        let output = b"\x05\0\0\0\x04\0\xff\x1b\n";
        let (uploaded, upload_observed) = tokio::sync::oneshot::channel();
        let (close, close_requested) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &Request, mut response: Response| {
                    assert_eq!(
                        request.headers()["sec-websocket-protocol"],
                        "st3.client.pty.v0, st3.cap.foreground-test",
                    );
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        "st3.client.pty.v0".parse().unwrap(),
                    );
                    Ok(response)
                },
            )
            .await
            .unwrap();
            websocket
                .send(Message::Binary(output.as_slice().into()))
                .await
                .unwrap();
            let mut received = Vec::new();
            while received.len() < upload.len() {
                let Message::Binary(bytes) = websocket.next().await.unwrap().unwrap() else {
                    panic!("PTY input must be binary, never selected-use text");
                };
                received.extend_from_slice(&bytes);
            }
            assert_eq!(received.as_slice(), upload.as_slice());
            uploaded.send(()).unwrap();
            for sequence in [1_u64, 2] {
                let Message::Text(text) = websocket.next().await.unwrap().unwrap() else {
                    panic!("selected-use must be WebSocket text, never PTY bytes");
                };
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&text).unwrap(),
                    json!({"type": "selected-use", "sequence": sequence}),
                );
            }
            close_requested.await.unwrap();
            websocket
                .send(Message::Text(
                    json!({"type": "error", "code": "lease_expired"})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            // Keep the server socket open until the client closes in response to text.
            // That makes EOF proof of the SDK boundary rather than server socket teardown.
            if let Some(message) = websocket.next().await {
                match message {
                    Ok(Message::Close(_)) | Err(_) => {}
                    other => panic!("unexpected client message after lease error: {other:?}"),
                }
            }
        });
        let controlled = Client::unix(&socket)
            .raw_terminal_stream_controlled(&RawTerminalAttachment {
                terminal_id: "terminal/foreground-test".into(),
                runtime_incarnation: "incarnation-test".into(),
                owner_host_id: "owner-test".into(),
                mode: RawTerminalMode::Peek,
                stream_capability: "foreground-test".into(),
            })
            .await
            .unwrap();
        let mut stream = controlled.stream;
        let activity = controlled.activity;
        let clone = activity.clone();
        let mut received = [0_u8; 9];
        stream.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, output);
        stream.write_all(upload).await.unwrap();
        upload_observed.await.unwrap();
        let (first, second) = tokio::join!(activity.selected_use(), clone.selected_use());
        first.unwrap();
        second.unwrap();
        close.send(()).unwrap();
        assert_eq!(stream.read(&mut [0_u8; 1]).await.unwrap(), 0);
        assert!(activity.selected_use().await.is_err());
        assert!(clone.selected_use().await.is_err());
        server.await.unwrap();
    })
    .await
    .expect("raw terminal controls and closure must complete without retaining the bridge");
}

#[allow(clippy::result_large_err)]
#[tokio::test]
async fn attach_selected_use_is_rejected_without_closing_pty() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("raw-attach.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_hdr_async(
                stream,
                |_: &Request, mut response: Response| {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        "st3.client.pty.v0".parse().unwrap(),
                    );
                    Ok(response)
                },
            )
            .await
            .unwrap();
            match websocket.next().await.unwrap().unwrap() {
                Message::Binary(bytes) => assert_eq!(bytes.as_ref(), b"foreground-input"),
                message => panic!("ATTACH must not send a renewal control: {message:?}"),
            }
            websocket.send(Message::Binary(b"terminal-output".as_slice().into())).await.unwrap();
            while websocket.next().await.is_some() {}
        });
        let controlled = Client::unix(&socket)
            .raw_terminal_stream_controlled(&RawTerminalAttachment {
                terminal_id: "terminal/attach-test".into(),
                runtime_incarnation: "incarnation-test".into(),
                owner_host_id: "owner-test".into(),
                mode: RawTerminalMode::Attach,
                stream_capability: "attach-test".into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            controlled.activity.selected_use().await,
            Err(st3_client::ClientError::Protocol(_)),
        ));
        let mut stream = controlled.stream;
        stream.write_all(b"foreground-input").await.unwrap();
        let mut output = [0_u8; 15];
        stream.read_exact(&mut output).await.unwrap();
        assert_eq!(&output, b"terminal-output");
        drop(stream);
        server.await.unwrap();
    })
    .await
    .expect("rejecting ATTACH renewal must preserve its PTY stream");
}
