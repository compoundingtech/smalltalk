//! What the replication HTTP client reports for each way an exchange can fail, at the pinned
//! reqwest 0.12.28, hyper 1.11.0 and hyper-util 0.1.20 (Cargo.lock). Retry policy reads none of
//! this yet. These controls pin the phase each failure reaches (none, connected, response head,
//! body) and how reqwest reports it, so a policy that tells an unreachable peer from a refusing or
//! stalled one can rest on observed behaviour rather than on an error's speed.
//!
//! A failure with no response head says only that this node saw no response. The peer may have
//! accepted the request and acted on it before the connection was lost.
//!
//! Every attempt, whatever its outcome, first exports and signs its payload (`exchange_inner`
//! exports, `post_signed_to` serializes, digests and signs before `send`). A policy that retries
//! sooner repeats that local work, including any sealing export does under the writer.
//!
//! A passing run records only that each observation fell inside the asserted set. Which branch
//! it took (an incomplete message or a reset, for the closing link) is printed only when an
//! assertion fails: nextest keeps a passing test's output only on failure, and the hosted
//! test-log artifacts do not include this module.
//!
//! TLS is not covered. Replication routes in these fleets are plain HTTP (loopback, Fabric
//! tunnels, configured `http://` peers); a TLS handshake failure must not be assumed to classify
//! like any case here.
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpSocket};

use super::*;

/// How far an exchange got, and how reqwest reports where it stopped.
#[derive(Debug)]
struct Phase {
    connect: bool,
    timeout: bool,
    body: bool,
    /// reqwest reports a failure reading a response body after its head as a decode error
    /// (`Response::bytes` maps it through `error::decode` in 0.12.28), not as a body error.
    decode: bool,
    /// The kind of each `std::io::Error` in the cause chain, outermost first.
    io: Vec<std::io::ErrorKind>,
    /// Every cause, outermost first.
    chain: Vec<String>,
}

impl Phase {
    fn of(error: &reqwest::Error) -> Self {
        let mut io = Vec::new();
        let mut chain = Vec::new();
        let mut cause: Option<&dyn std::error::Error> = Some(error);
        while let Some(current) = cause {
            chain.push(current.to_string());
            if let Some(found) = current.downcast_ref::<std::io::Error>() {
                io.push(found.kind());
            }
            cause = current.source();
        }
        Self {
            connect: error.is_connect(),
            timeout: error.is_timeout(),
            body: error.is_body(),
            decode: error.is_decode(),
            io,
            chain,
        }
    }

    fn says(&self, text: &str) -> bool {
        self.chain.iter().any(|cause| cause.contains(text))
    }
}

/// One exchange-shaped POST through the replication client, as `post_signed_to` sends it: a
/// body large enough to still be in flight when the far side closes.
async fn exchange(
    address: std::net::SocketAddr,
    timeout: Option<Duration>,
) -> reqwest::Result<reqwest::Response> {
    let mut request = replication_http_client()
        .post(format!("http://{address}{EXCHANGE_PATH}"))
        .header("content-type", "application/json")
        .body(vec![b'x'; 64 * 1024]);
    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }
    request.send().await
}

/// Read one request exactly: its head, then the content-length bytes of its body. A connection
/// closed with request bytes still unread is reset, and a reset can destroy a response head
/// before the client reads it.
async fn read_request(stream: &mut tokio::net::TcpStream) -> usize {
    let mut received = Vec::new();
    let mut buffer = [0_u8; 8192];
    let head_end = loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0, "the request ended before its head");
        received.extend_from_slice(&buffer[..count]);
        if let Some(at) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&received[..head_end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map(|value| value.trim().parse().unwrap())
        .unwrap_or(0);
    let mut body = received.len() - head_end;
    while body < length {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0, "the request ended before its body");
        body += count;
    }
    length
}

#[tokio::test]
async fn a_refused_connection_is_a_connect_error() {
    // A socket bound without listening holds its port and refuses every connection.
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let address = socket.local_addr().unwrap();
    let seen = Phase::of(&exchange(address, None).await.unwrap_err());
    assert!(
        seen.connect && !seen.timeout && !seen.body && !seen.decode,
        "{seen:#?}"
    );
    assert!(
        seen.io.contains(&std::io::ErrorKind::ConnectionRefused),
        "{seen:#?}"
    );
}

/// The messaging-faults link while disabled (`scripts/st3-messaging-faults-eval/run`,
/// `Link.listen`): it accepts the connection and closes it without forwarding a byte. The held
/// link_minutes failures were this, not a refused connection.
#[tokio::test]
async fn a_link_that_accepts_and_closes_is_not_a_connect_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let link = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        }
    });
    let mut seen = Vec::new();
    for _ in 0..8 {
        seen.push(Phase::of(&exchange(address, None).await.unwrap_err()));
    }
    link.abort();
    for one in &seen {
        assert!(
            !one.connect && !one.timeout && !one.body && !one.decode,
            "{seen:#?}"
        );
        // The close reaches the client as an end of stream before any response (hyper's
        // incomplete message) or, when the request bytes arrived first, as a reset, abort or
        // early end of file, depending on the kernel and timing. All are this class; anything
        // else means the pinned stack changed.
        assert!(
            one.says("connection closed before message completed")
                || one.io.iter().any(|kind| matches!(
                    kind,
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::UnexpectedEof
                )),
            "{seen:#?}"
        );
    }
}

#[tokio::test]
async fn a_peer_that_refuses_or_is_overloaded_answers_with_a_response_head() {
    let app = axum::Router::new()
        .route(
            "/refuse/v1/peer/exchange",
            // Each handler reads the whole body first: answering with request bytes unread would
            // let the close become a reset that can destroy the response head.
            axum::routing::post(|_: axum::body::Bytes| async {
                axum::http::StatusCode::UNAUTHORIZED
            }),
        )
        .route(
            "/overloaded/v1/peer/exchange",
            axum::routing::post(|_: axum::body::Bytes| async {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    [(axum::http::header::RETRY_AFTER, "7")],
                )
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let client = replication_http_client();
    let refused = client
        .post(format!("http://{address}/refuse{EXCHANGE_PATH}"))
        .body(vec![b'x'; 64 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED);
    let overloaded = client
        .post(format!("http://{address}/overloaded{EXCHANGE_PATH}"))
        .body(vec![b'x'; 64 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(
        overloaded.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        overloaded
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .unwrap(),
        "7"
    );
    server.abort();
}

/// A peer that accepts and reads the whole request but never answers. The server reports when it
/// has read the request, so a failure to connect in time cannot pass as a stall after
/// connecting. The 2 s per-request timeout stands in for `REPLICATION_EXCHANGE_TIMEOUT` (20 s);
/// it is a test substitution, not a measurement of the 20 s path.
#[tokio::test]
async fn a_peer_that_stalls_before_its_response_head_is_a_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (read, request_read) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let length = read_request(&mut stream).await;
        let _ = read.send(length);
        // Hold the connection open without answering.
        std::future::pending::<()>().await;
    });
    let result = exchange(address, Some(Duration::from_secs(2))).await;
    peer.abort();
    let length = request_read.await;
    let seen = Phase::of(&result.unwrap_err());
    assert_eq!(
        length,
        Ok(64 * 1024),
        "the peer read the whole request: {seen:#?}"
    );
    assert!(
        seen.timeout && !seen.connect && !seen.body && !seen.decode,
        "{seen:#?}"
    );
}

/// A peer that sends a response head and then loses the connection mid-body: `send` succeeds,
/// and the failure comes from reading the body.
#[tokio::test]
async fn a_response_cut_off_after_its_head_fails_reading_the_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let length = read_request(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\npartial")
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
        length
    });
    let response = exchange(address, None).await.unwrap();
    let status = response.status();
    let result = response.bytes().await;
    assert_eq!(
        peer.await.unwrap(),
        64 * 1024,
        "the peer read the whole request"
    );
    assert_eq!(status, reqwest::StatusCode::OK);
    let seen = Phase::of(&result.unwrap_err());
    assert!(
        seen.decode && !seen.body && !seen.connect && !seen.timeout,
        "{seen:#?}"
    );
}
