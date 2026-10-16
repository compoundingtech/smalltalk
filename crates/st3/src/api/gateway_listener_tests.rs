#[tokio::test]
async fn live_listener_cannot_be_unlinked_by_another_daemon() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("gateway.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let error = super::serve_unix(&socket, axum::Router::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("refusing to replace live Unix socket listener"),
        "{error}"
    );
    assert!(tokio::net::UnixStream::connect(&socket).await.is_ok());
    drop(listener);
}

#[tokio::test]
async fn reserved_addresses_refuse_connections_until_initialized_serving() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("api.sock");
    let discovery = root.path().join("state/run/st3.sock");
    let reserved = super::reserve_unix_socket(&socket, Some(&discovery))
        .await
        .unwrap();
    assert!(socket.exists());
    for path in [&socket, &discovery] {
        let error = tokio::net::UnixStream::connect(path).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
    }

    let (ready, serving) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(super::serve_reserved_unix_with_ready(
        reserved,
        axum::Router::new().fallback(|| async { "initialized" }),
        false,
        move || ready.send(()).unwrap(),
    ));
    serving.await.unwrap();
    let error = super::reserve_unix_socket(&socket, None)
        .await
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("refusing to replace reserved Unix socket"),
        "{error}"
    );
    let mut stream = tokio::net::UnixStream::connect(&discovery).await.unwrap();
    stream
        .write_all(b"GET /ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_string(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("initialized"), "{response}");
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn a_starting_socket_reservation_cannot_be_unlinked_even_through_discovery() {
    use std::os::unix::fs::MetadataExt as _;
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("api.sock");
    let discovery = root.path().join("state/run/st3.sock");
    let reserved = super::reserve_unix_socket(&socket, Some(&discovery))
        .await
        .unwrap();
    let inode = std::fs::metadata(&socket).unwrap().ino();
    for path in [&socket, &discovery] {
        let error = super::reserve_unix_socket(path, None).await.err().unwrap();
        assert!(
            error
                .to_string()
                .contains("refusing to replace reserved Unix socket"),
            "{error}"
        );
        assert_eq!(std::fs::metadata(path).unwrap().ino(), inode);
    }
    drop(reserved);
    // A released/crashed reservation is not a permanent latch.
    let next = super::reserve_unix_socket(&socket, Some(&discovery))
        .await
        .unwrap();
    assert!(tokio::net::UnixStream::connect(&socket).await.is_err());
    drop(next);
}
