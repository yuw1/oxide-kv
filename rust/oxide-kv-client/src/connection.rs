//! Low-level TCP + JSON line transport.
//!
//! One `Connection` owns a single TCP stream to a node. The wire
//! contract is one JSON object per line, `\n`-terminated; this is
//! how the server's `ClientHandler` in `rust/oxide-kv/src/client.rs`
//! reads commands. The connection is **not** thread-safe — wrap with
//! a lock or open one `Connection` per task.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::error::{Error, Result};

/// A JSON-line TCP connection to an Oxide-KV node.
///
/// `send_request` serializes the value to JSON, appends `\n`, writes
/// it, then reads a single line back and deserializes. The server
/// always replies with exactly one line per request, so a single
/// `read_line` is sufficient.
pub struct Connection {
    reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
    writer: tokio::io::WriteHalf<TcpStream>,
}

impl Connection {
    /// Open a new connection to `host:port`.
    pub async fn connect(host: &str, port: u16) -> Result<Self> {
        let addr = format!("{host}:{port}");
        let stream = TcpStream::connect(&addr).await?;
        let (read_half, write_half) = tokio::io::split(stream);
        Ok(Self {
            reader: BufReader::new(read_half),
            writer: write_half,
        })
    }

    /// Send a JSON-serializable request and read back a JSON response.
    ///
    /// Blocks until the reply line arrives. The server writes exactly
    /// one line per request, so this terminates as soon as `\n` is
    /// seen. The caller is responsible for whatever encoding makes
    /// sense on top of this transport.
    pub async fn send_request(&mut self, request: &serde_json::Value) -> Result<serde_json::Value> {
        // 1. Serialize + write the request line.
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await?;

        // 2. Read one reply line.
        let mut buf = String::new();
        let n = self.reader.read_line(&mut buf).await?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }

        // 3. Deserialize.
        serde_json::from_str(&buf).map_err(Error::from)
    }
}
