//! Bounded arbitrary-byte checks over the same decoders used by TCP consumers.
//!
//! References:
//! - [RTSP 1.0](https://www.rfc-editor.org/rfc/rfc2326)
//! - [RTP H.264](https://www.rfc-editor.org/rfc/rfc6184)

use super::client::*;
use proptest::prelude::*;
use tokio::io::AsyncWriteExt;

/// Build an in-memory, finite byte stream with EOF, never a socket or timed writer.
async fn bytes_client(bytes: &[u8]) -> Client<tokio::io::DuplexStream> {
    let (socket, mut writer) = tokio::io::duplex(bytes.len().max(1));
    writer.write_all(bytes).await.unwrap();
    writer.shutdown().await.unwrap();
    Client::from_io(socket)
}

/// Supply only the time driver required by decoder deadlines.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

proptest! {
    #[test]
    fn reply_never_panics_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        runtime().block_on(async { let _ = bytes_client(&bytes).await.reply().await; });
    }
    #[test]
    fn interleaved_and_frame_never_panic_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        runtime().block_on(async {
            let _ = bytes_client(&bytes).await.packet(tokio::time::Instant::now() + std::time::Duration::from_millis(50)).await;
            let _ = bytes_client(&bytes).await.frame(26).await;
        });
    }
    #[test]
    fn annex_b_never_panics_on_arbitrary_packets(packets in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..256), 0..32)) {
        let limit = packets.iter().map(Vec::len).sum::<usize>() + packets.len() * 4;
        if let Ok(bytes) = annex_b(&packets) { prop_assert!(bytes.len() <= limit); }
    }
    #[test]
    fn nonce_never_panics_on_arbitrary_text(value in ".{0,2048}") {
        let _ = nonce(&value);
    }
}

#[test]
fn malformed_and_oversized_replies_fail_without_waiting_or_allocating_the_body() {
    let mut cases = vec![
        b"HTTP/1.1 invalid\r\n\r\n".to_vec(),
        b"RTSP/1.0 200 OK\r\nmalformed\r\n\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: invalid\r\n\r\n".to_vec(),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        )
        .into_bytes(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nshort".to_vec(),
        vec![b'x'; MAX_HEAD + 1],
        vec![0xff, b'\r', b'\n', b'\r', b'\n'],
    ];
    cases.push(b"$\0\0\x08short".to_vec());
    runtime().block_on(async {
        for bytes in cases {
            assert!(bytes_client(&bytes).await.reply().await.is_err());
        }
    });
}
