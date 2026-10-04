//! Generic exposure wire at fabric v0.2.30 / 8bd9017. No mux and no resumption.
use anyhow::{Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: usize = 1024 * 1024;
pub const WINDOW: u64 = 4 * 1024 * 1024;
pub const CHUNK: usize = 8192;

#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    Hello {
        id: [u8; 16],
        next: u64,
        resume: bool,
    },
    Data {
        offset: u64,
        bytes: Vec<u8>,
    },
    Ack(u64),
    Close(u64),
    Error(String),
}

impl Frame {
    fn encode(self) -> (u8, Vec<u8>) {
        match self {
            Self::Hello { id, next, resume } => {
                let mut bytes = id.to_vec();
                bytes.extend(next.to_be_bytes());
                bytes.push(u8::from(resume));
                (1, bytes)
            }
            Self::Data { offset, bytes } => {
                let mut body = offset.to_be_bytes().to_vec();
                body.extend(bytes);
                (2, body)
            }
            Self::Ack(next) => (3, next.to_be_bytes().to_vec()),
            Self::Close(offset) => (4, offset.to_be_bytes().to_vec()),
            Self::Error(message) => (5, message.into_bytes()),
        }
    }
}

pub async fn write<W: AsyncWrite + Unpin>(out: &mut W, frame: Frame) -> Result<()> {
    let (kind, bytes) = frame.encode();
    if bytes.len() > MAX_FRAME {
        bail!("fabric frame exceeds limit");
    }
    out.write_u8(kind).await?;
    out.write_u32(bytes.len() as u32).await?;
    out.write_all(&bytes).await?;
    out.flush().await?;
    Ok(())
}

pub async fn read<R: AsyncRead + Unpin>(input: &mut R) -> Result<Frame> {
    let kind = input.read_u8().await?;
    let length = input.read_u32().await? as usize;
    if length > MAX_FRAME {
        bail!("fabric frame exceeds limit");
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body).await?;
    let integer = |bytes: &[u8]| u64::from_be_bytes(bytes.try_into().expect("checked length"));
    match (kind, length) {
        (1, 24 | 25) => Ok(Frame::Hello {
            id: body[..16].try_into()?,
            next: integer(&body[16..24]),
            resume: body.get(24).is_some_and(|byte| *byte != 0),
        }),
        (2, 8..) => Ok(Frame::Data {
            offset: integer(&body[..8]),
            bytes: body[8..].to_vec(),
        }),
        (3, 8) => Ok(Frame::Ack(integer(&body))),
        (4, 8) => Ok(Frame::Close(integer(&body))),
        (5, _) => Ok(Frame::Error(String::from_utf8_lossy(&body).into_owned())),
        _ => bail!("invalid fabric frame kind or length"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    // Literal fixtures independent of the encoder, read through fragmented I/O.
    #[tokio::test]
    async fn pinned_frames_and_bounds() -> Result<()> {
        let (mut sender, mut receiver) = duplex(3);
        let fixture = [1, 0, 0, 0, 25]
            .into_iter()
            .chain([7; 16])
            .chain([0; 8])
            .chain([0])
            .collect::<Vec<_>>();
        let sent = tokio::spawn(async move { sender.write_all(&fixture).await });
        assert_eq!(
            read(&mut receiver).await?,
            Frame::Hello {
                id: [7; 16],
                next: 0,
                resume: false
            }
        );
        sent.await??;
        let mut old = [1, 0, 0, 0, 24]
            .into_iter()
            .chain([2; 16])
            .chain([0; 8])
            .collect::<Vec<_>>();
        assert_eq!(
            read(&mut old.as_slice()).await?,
            Frame::Hello {
                id: [2; 16],
                next: 0,
                resume: false
            }
        );
        old[4] = 23;
        assert!(read(&mut old.as_slice()).await.is_err());
        assert!(read(&mut [2, 0, 16, 0, 1].as_slice()).await.is_err());
        assert!(read(&mut [9, 0, 0, 0, 0].as_slice()).await.is_err());
        let mut bytes = Vec::new();
        write(&mut bytes, Frame::Close(3)).await?;
        assert_eq!(bytes, [4, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 3]);
        Ok(())
    }
}
