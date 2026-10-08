//! Byte bounds at the signed transport boundary, before parsing or retaining an answer.
use super::*;
use std::io::{self, Write};

struct LimitedVec {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for LimitedVec {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "replication body exceeds its byte limit",
            ));
        }
        let needed = self.bytes.len() + bytes.len();
        if needed > self.bytes.capacity() {
            // Vec's implicit doubling can reserve beyond the byte cap on the last write.
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(needed)
                .min(self.limit);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>> {
    let mut writer = LimitedVec {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value).context("encode bounded replication body")?;
    Ok(writer.bytes)
}

pub(super) async fn read(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= limit as u64,
            "replication response exceeds {limit} bytes"
        );
    }
    let mut writer = LimitedVec {
        bytes: Vec::new(),
        limit,
    };
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read replication response chunk")?
    {
        writer
            .write_all(&chunk)
            .context("bound replication response bytes")?;
    }
    Ok(writer.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_limit_preserves_exact_bytes_and_stops_before_buffer_growth() {
        let value = serde_json::json!({"payload": "a".repeat(4096)});
        let expected = serde_json::to_vec(&value).unwrap();
        assert_eq!(encode(&value, expected.len()).unwrap(), expected);
        assert!(encode(&value, expected.len() - 1).is_err());
        let mut writer = LimitedVec {
            bytes: Vec::new(),
            limit: 8,
        };
        assert!(writer.write_all(&[0; 9]).is_err());
        assert!(writer.bytes.is_empty());
    }

    #[tokio::test]
    async fn response_limit_covers_declared_and_chunked_bodies() {
        use axum::{Router, body::Body, routing::get};
        use std::convert::Infallible;
        let app = Router::new()
            .route("/declared", get(|| async { vec![0u8; 65] }))
            .route(
                "/chunked",
                get(|| async {
                    Body::from_stream(futures_util::stream::iter(
                        (0..9)
                            .map(|_| Ok::<_, Infallible>(axum::body::Bytes::from_static(&[0; 8]))),
                    ))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        let client = reqwest::Client::new();
        let declared = read(
            client
                .get(format!("http://{address}/declared"))
                .send()
                .await
                .unwrap(),
            64,
        )
        .await;
        let chunked = read(
            client
                .get(format!("http://{address}/chunked"))
                .send()
                .await
                .unwrap(),
            64,
        )
        .await;
        let exact = read(
            client
                .get(format!("http://{address}/declared"))
                .send()
                .await
                .unwrap(),
            65,
        )
        .await;
        stop.send(()).unwrap();
        server.await.unwrap();
        assert!(declared.is_err());
        assert!(chunked.is_err());
        assert_eq!(exact.unwrap(), vec![0; 65]);
    }
}
