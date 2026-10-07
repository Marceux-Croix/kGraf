//! The console: a progress line redrawn four times a second by a background
//! thread, and log lines printed above it.
//!
//! With virtual terminal processing on (Windows Terminal, and conhost once
//! `SetConsoleMode` enables it) the line is cleared with `ESC[2K`; without
//! it the line is overwritten with a carriage return and spaces, which every
//! console understands.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::progress::Progress;

#[derive(Clone)]
pub struct Console {
    pub progress: Arc<Mutex<Progress>>,
    start: Instant,
    ansi: bool,
    width: usize,
    stop: Arc<AtomicBool>,
    out_lock: Arc<Mutex<()>>,
}

impl Console {
    pub fn new(progress: Progress) -> Self {
        let ansi = crate::platform::enable_vt();
        Self {
            progress: Arc::new(Mutex::new(progress)),
            start: Instant::now(),
            ansi,
            width: crate::platform::console_width(),
            stop: Arc::new(AtomicBool::new(false)),
            out_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn clear(&self, out: &mut impl Write) {
        if self.ansi {
            let _ = write!(out, "\r\x1b[2K");
        } else {
            let _ = write!(out, "\r{}\r", " ".repeat(self.width.saturating_sub(1)));
        }
    }

    fn draw(&self, tick: usize) {
        let _g = self.out_lock.lock();
        let line = match self.progress.lock() {
            Ok(p) => p.render(self.now(), tick, self.width),
            Err(_) => return,
        };
        let mut out = std::io::stdout().lock();
        self.clear(&mut out);
        let _ = write!(out, "{line}");
        let _ = out.flush();
    }

    /// Start the redraw thread.
    pub fn spawn_ticker(&self) -> std::thread::JoinHandle<()> {
        let me = self.clone();
        std::thread::spawn(move || {
            let mut tick = 0usize;
            while !me.stop.load(Ordering::Relaxed) {
                me.draw(tick);
                tick = tick.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(250));
            }
        })
    }

    /// Print a line above the progress line.
    pub fn log(&self, text: &str) {
        let _g = self.out_lock.lock();
        let mut out = std::io::stdout().lock();
        self.clear(&mut out);
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }

    /// Stop redrawing and leave the last line in place.
    pub fn stop(&self, ticker: std::thread::JoinHandle<()>) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = ticker.join();
        self.draw(0);
        println!();
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Progress, f64) -> R) -> Option<R> {
        let now = self.now();
        self.progress.lock().ok().map(|mut p| f(&mut p, now))
    }
}
