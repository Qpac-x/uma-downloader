//! Live view: a fixed pool of spinner bars showing the files currently being
//! downloaded, coordinated through an indicatif MultiProgress.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

pub struct LiveView {
    free: Arc<Mutex<VecDeque<ProgressBar>>>,
}

impl LiveView {
    /// Create `workers` spinner bars (hidden until acquired) inside `mp`.
    pub fn new(mp: &MultiProgress, workers: usize) -> Self {
        let style = ProgressStyle::with_template("  {spinner:.cyan} {msg}")
            .unwrap()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠸");
        let mut free = VecDeque::with_capacity(workers);
        for _ in 0..workers {
            let pb = mp.add(ProgressBar::new_spinner());
            pb.set_style(style.clone());
            pb.enable_steady_tick(Duration::from_millis(80));
            free.push_back(pb);
        }
        Self {
            free: Arc::new(Mutex::new(free)),
        }
    }

    /// Borrow a spinner for the lifetime of one download. Returns `None` if the
    /// pool is empty (only possible when concurrency exceeds `workers`).
    pub fn acquire(&self) -> Option<Slot> {
        let pb = self.free.lock().unwrap().pop_front()?;
        Some(Slot {
            pb,
            free: Arc::clone(&self.free),
        })
    }
}

pub struct Slot {
    pb: ProgressBar,
    free: Arc<Mutex<VecDeque<ProgressBar>>>,
}

impl Slot {
    pub fn set_file(&self, name: &str) {
        self.pb.set_message(elide(name, 60));
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.pb.set_message(String::new());
        self.free.lock().unwrap().push_back(self.pb.clone());
    }
}

/// Truncate to the tail `max` chars (so long asset paths don't wrap the line).
fn elide(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let start = chars.len() - max;
        format!("…{}", chars[start..].iter().collect::<String>())
    }
}
