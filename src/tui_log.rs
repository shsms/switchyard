//! Placeholder for the TUI log buffer; the headless binary doesn't use
//! it, but the module path is referenced by future TUI work and we
//! keep it stable from day one.

use std::{collections::VecDeque, sync::Arc};

use parking_lot::Mutex;

#[derive(Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<String>>>,
    cap: usize,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(cap))),
            cap,
        }
    }

    pub fn push(&self, line: String) {
        let mut g = self.inner.lock();
        if g.len() >= self.cap {
            g.pop_front();
        }
        g.push_back(line);
    }

    pub fn lines(&self) -> Vec<String> {
        self.inner.lock().iter().cloned().collect()
    }
}
