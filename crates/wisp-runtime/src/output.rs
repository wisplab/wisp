//! Bounded output capture that can be drained between session evals.
//! Once full, discard further bytes rather than closing the WASI stream:
//! Python can keep flushing, and the next eval starts with an empty buffer.

use bytes::Bytes;
use std::sync::{Arc, Mutex};
use wasmtime_wasi::{HostOutputStream, StdoutStream, StreamResult, Subscribe};

#[derive(Default)]
struct Buffer {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Clone)]
pub(crate) struct OutputCapture {
    capacity: usize,
    buffer: Arc<Mutex<Buffer>>,
}

impl OutputCapture {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            buffer: Arc::new(Mutex::new(Buffer::default())),
        }
    }

    pub(crate) fn take(&self) -> String {
        let buffer = std::mem::take(&mut *self.buffer.lock().unwrap());
        let mut text = String::from_utf8_lossy(&buffer.bytes).into_owned();
        if buffer.truncated {
            text.push_str(&format!(
                "\n[wisp: output truncated at {} bytes]\n",
                self.capacity
            ));
        }
        text
    }
}

impl StdoutStream for OutputCapture {
    fn stream(&self) -> Box<dyn HostOutputStream> {
        Box::new(self.clone())
    }
    fn isatty(&self) -> bool {
        false
    }
}

#[wasmtime_wasi::async_trait]
impl Subscribe for OutputCapture {
    async fn ready(&mut self) {}
}

#[wasmtime_wasi::async_trait]
impl HostOutputStream for OutputCapture {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        let mut buffer = self.buffer.lock().unwrap();
        let keep = bytes
            .len()
            .min(self.capacity.saturating_sub(buffer.bytes.len()));
        buffer.bytes.extend_from_slice(&bytes[..keep]);
        buffer.truncated |= keep < bytes.len();
        Ok(())
    }
    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }
    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(64 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_is_bounded_and_drain_restores_capacity() {
        let mut output = OutputCapture::new(4);
        output.write(Bytes::from_static(b"abc")).unwrap();
        output.write(Bytes::from_static(b"def")).unwrap();
        assert_eq!(output.buffer.lock().unwrap().bytes.len(), 4);
        assert!(output.check_write().unwrap() > 0);
        assert_eq!(output.take(), "abcd\n[wisp: output truncated at 4 bytes]\n");
        output.write(Bytes::from_static(b"next")).unwrap();
        assert_eq!(output.take(), "next");
        assert_eq!(output.take(), "");
    }
}
