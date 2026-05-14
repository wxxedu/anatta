//! Rolling stderr buffer — capture last N bytes for diagnostics on exit.

use std::sync::{Arc, Mutex};

const MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Handle {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl Handle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append(&self, bytes: &[u8]) {
        let Ok(mut buf) = self.inner.lock() else {
            return;
        };
        buf.extend_from_slice(bytes);
        // Roll: keep only the trailing MAX_BYTES. Cheap because we only
        // copy when over budget.
        if buf.len() > MAX_BYTES {
            let drop_n = buf.len() - MAX_BYTES;
            buf.drain(..drop_n);
        }
    }

    pub fn snapshot(&self) -> String {
        let Ok(buf) = self.inner.lock() else {
            return String::new();
        };
        String::from_utf8_lossy(&buf).into_owned()
    }
}
