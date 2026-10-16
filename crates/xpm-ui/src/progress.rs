use crate::UiMessage;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn request_cancel() {
    CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

pub(crate) fn cancel_requested() -> bool {
    CANCEL_REQUESTED.load(Ordering::SeqCst)
}

const CAPTURE_LIMIT: usize = 65536;
const FLUSH_INTERVAL: Duration = Duration::from_millis(60);

const PACMAN_USER_PROMPT_PATTERNS: &[&str] = &[
    "Proceed with installation? [Y/n]",
    "Proceed with download? [Y/n]",
    ":: Proceed with installation? [Y/n]",
    ":: Proceed with download? [Y/n]",
    "Do you want to remove these packages? [y/N]",
    ":: Do you want to remove these packages? [y/N]",
    ":: Replace",
    ":: Import",
    "Enter a number",
    "Enter number to select",
    "Enter a selection",
    "Terminate batch job",
    "Which do you want to install",
    "Which do you want to use",
    "(0 to abort)",
    "Choose identity to authenticate",
];

/// Map typographic Unicode (curly quotes, dashes, ellipsis, bidi marks) to plain
/// ASCII so the monospace terminal popup doesn't render them as '?'.
fn normalize_typographic(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => out.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => out.push('"'),
            '\u{2013}' | '\u{2014}' | '\u{2015}' => out.push('-'),
            '\u{2026}' => out.push_str("..."),
            '\u{00A0}' | '\u{202F}' => out.push(' '),
            '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => {}
            _ => out.push(c),
        }
    }
    out
}

fn strip_ansi(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '\x1b' {
            i += 1;
            if i >= len { break; }
            match chars[i] {
                '[' => {
                    i += 1;
                    while i < len && (chars[i] >= '0' && chars[i] <= '?') { i += 1; }
                    while i < len && (chars[i] >= ' ' && chars[i] <= '/') { i += 1; }
                    if i < len && (chars[i] >= '@' && chars[i] <= '~') { i += 1; }
                }
                ']' => {
                    i += 1;
                    while i < len {
                        if chars[i] == '\x07' { i += 1; break; }
                        if chars[i] == '\x1b' && i + 1 < len && chars[i + 1] == '\\' {
                            i += 2; break;
                        }
                        i += 1;
                    }
                }
                '(' | ')' | '*' | '+' => {
                    i += 1;
                    if i < len { i += 1; }
                }
                _ => { i += 1; }
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

struct TermStream {
    lines: Vec<String>,
    current: String,
    pending_cr: bool,
    in_csi: bool,
    in_osc: bool,
    osc_st: bool,
}

impl TermStream {
    fn new() -> Self {
        Self { lines: Vec::new(), current: String::new(), pending_cr: false, in_csi: false, in_osc: false, osc_st: false }
    }

    fn process(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];

            if self.in_csi {
                if (0x40..=0x7e).contains(&b) { self.in_csi = false; }
                i += 1;
                continue;
            }
            if self.osc_st {
                self.osc_st = false;
                if b == b'\\' {
                    i += 1;
                    continue;
                }
            }
            if self.in_osc {
                if b == 0x07 { self.in_osc = false; }
                if b == 0x1b { self.in_osc = false; self.osc_st = true; }
                i += 1;
                continue;
            }

            if self.pending_cr {
                self.pending_cr = false;
                if b == 0x0a {
                    self.lines.push(std::mem::take(&mut self.current));
                    i += 1;
                    continue;
                }
                self.current.clear();
            }

            match b {
                0x1b => {
                    i += 1;
                    if i < bytes.len() {
                        match bytes[i] {
                            b'[' => { self.in_csi = true; i += 1; }
                            b']' => { self.in_osc = true; i += 1; }
                            b'(' | b')' | b'*' | b'+' => { i += 2; }
                            _ => { i += 1; }
                        }
                    }
                }
                0x0d => { self.pending_cr = true; i += 1; }
                0x0a => { self.lines.push(std::mem::take(&mut self.current)); i += 1; }
                0x08 => { self.current.pop(); i += 1; }
                0x00..=0x07 | 0x0b | 0x0c | 0x0e..=0x1a | 0x1c..=0x1f | 0x7f => { i += 1; }
                _ => {
                    let char_len = match b {
                        0x00..=0x7f => 1usize,
                        0xc0..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf7 => 4,
                        _ => 1,
                    };
                    let end = (i + char_len).min(bytes.len());
                    if let Ok(s) = std::str::from_utf8(&bytes[i..end]) {
                        self.current.push_str(s);
                    }
                    i += (end - i).max(1);
                }
            }
        }
    }

    fn render(&self) -> String {
        let mut parts: Vec<&str> = self.lines.iter().map(String::as_str).collect();
        if !self.current.is_empty() {
            parts.push(self.current.as_str());
        }
        while parts.last().map(|s| s.trim().is_empty()).unwrap_or(false) {
            parts.pop();
        }
        if parts.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(parts.iter().map(|s| s.len() + 1).sum::<usize>() + 1);
        let mut prev_blank = false;
        for line in &parts {
            let is_blank = line.trim().is_empty();
            if is_blank && prev_blank { continue; }
            prev_blank = is_blank;
            out.push_str(line);
            out.push('\n');
        }
        normalize_typographic(&out)
    }
}

fn classify_log_level(lower: &str) -> u8 {
    if lower.contains("error:") { 1 }
    else if lower.contains("warning:") { 2 }
    else if (lower.contains("installed") || lower.contains("upgraded") || lower.contains("removed"))
        && !lower.contains("error:") { 3 }
    else { 0 }
}

/// Extract the last `XX%` token from a line (pacman inline progress indicator).
fn extract_inline_percent(raw: &str) -> Option<i32> {
    raw.split_whitespace()
        .filter_map(|t| t.strip_suffix('%')?.parse::<i32>().ok().filter(|&p| (0..=100).contains(&p)))
        .next_back()
}

/// Parse `(k/N)` from a pacman progress line and return fractional progress
/// `(k-1 + inline%/100) / N` in [0.0, 1.0].
fn parse_kn_fraction(raw: &str) -> Option<f32> {
    let s = raw.find('(')?;
    let e = raw[s..].find(')')?;
    let inner = &raw[s + 1..s + e];
    let mut parts = inner.split('/');
    let k: i32 = parts.next()?.trim().parse().ok()?;
    let n: i32 = parts.next()?.trim().parse().ok()?;
    if n <= 0 || k < 1 { return None; }
    let inline = extract_inline_percent(raw).unwrap_or(0) as f32;
    Some(((k - 1) as f32 + inline / 100.0) / n as f32)
}

fn detect_phase(lower: &str, raw: &str, _total_packages: usize) -> Option<(&'static str, i32)> {

    let is_install = lower.contains("installing") || lower.contains("upgrading")
        || lower.contains("reinstalling") || lower.contains("downgrading");
    let is_remove  = lower.contains("removing");
    if (is_install || is_remove)
        && let Some(frac) = parse_kn_fraction(raw) {
            let pct = (10.0 + frac * 85.0) as i32;
            let label = if is_remove { "Removing packages..." } else { "Installing packages..." };
            return Some((label, pct.min(95)));
        }

    if lower.contains("running pre-transaction hooks") || lower.contains("pre-transaction") {
        return Some(("Preparing...", 8));
    }

    if lower.contains("resolving dependencies") { return Some(("Resolving dependencies...", 1)); }
    if lower.contains("looking for conflicting") { return Some(("Checking for conflicts...", 2)); }

    let has_speed = lower.contains("mib/s") || lower.contains("kib/s")
        || lower.contains(" b/s") || lower.contains("mb/s") || lower.contains("kb/s");
    if has_speed {
        let pct = extract_inline_percent(raw).map(|p| 2 + p * 3 / 100).unwrap_or(2);
        return Some(("Downloading packages...", pct.min(5)));
    }
    if lower.contains("retrieving packages") || lower.contains("downloading") {
        return Some(("Downloading packages...", 2));
    }

    let is_verify = lower.contains("checking keyring") || lower.contains("checking keys")
        || lower.contains("checking integrity") || lower.contains("loading package files")
        || lower.contains("checking for file conflicts") || lower.contains("checking available disk");
    if is_verify {
        let pct = parse_kn_fraction(raw).map(|f| (5.0 + 3.0 * f) as i32).unwrap_or(5);
        return Some(("Verifying packages...", pct.min(8)));
    }

    if lower.contains("running post-transaction hooks") {
        return Some(("Running hooks...", 95));
    }
    if lower.contains("mkinitcpio") || lower.contains("updating linux initcpios") {
        return Some(("Rebuilding initramfs...", 97));
    }
    if lower.contains("grub-mkconfig") || lower.contains("grub") {
        return Some(("Updating bootloader...", 99));
    }
    if lower.contains("arming") || lower.contains("ldconfig") || lower.contains("dkms")
        || lower.contains("systemd") || lower.contains("fontconfig") || lower.contains("update-desktop")
        || lower.contains("gtk-update") || lower.contains("update-mime") {
        let pct = parse_kn_fraction(raw).map(|f| (95.0 + 4.0 * f) as i32).unwrap_or(96);
        return Some(("Running hooks...", pct.min(99)));
    }

    None
}


/// Detects interactive prompts and sends the appropriate UI messages.
/// Returns true if the output should be force-flushed to the UI immediately.
fn handle_pty_prompt(cleaned: &str, tx: &mpsc::Sender<UiMessage>) -> bool {
    let has_yn = cleaned.contains("[Y/n]") || cleaned.contains("[y/n]")
        || cleaned.contains("[Y|n]") || cleaned.contains("[y|n]");
    let has_y_n = cleaned.contains("[y/N]") || cleaned.contains("[y|N]");
    let needs_user_input = PACMAN_USER_PROMPT_PATTERNS.iter().any(|p| cleaned.contains(p)) || has_y_n;
    if needs_user_input || has_yn {
        let prompt_text = cleaned.lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or(cleaned)
            .trim()
            .to_string();
        if has_yn || has_y_n {
            let _ = tx.send(UiMessage::ProgressPromptButtons);
            let _ = tx.send(UiMessage::ProgressPrompt(prompt_text));
        } else {
            let _ = tx.send(UiMessage::ProgressPrompt(prompt_text));
            let _ = tx.send(UiMessage::ProgressAutoExpand);
        }
        true
    } else {
        false
    }
}

/// `NN.N% done` style progress (zsync / appimageupdatetool). Last one on the line wins.
fn extract_done_percent(lower: &str) -> Option<f32> {
    let idx = lower.rfind("% done")?;
    let head = &lower[..idx];
    let start = head
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    head[start..].parse::<f32>().ok().filter(|p| (0.0..=100.0).contains(p))
}

/// `[k/N]` counter used by flatpak.
fn parse_bracket_fraction(raw: &str) -> Option<f32> {
    let s = raw.find('[')?;
    let e = raw[s..].find(']')?;
    let (k, n) = raw[s + 1..s + e].split_once('/')?;
    let k: i32 = k.trim().parse().ok()?;
    let n: i32 = n.trim().parse().ok()?;
    if n <= 0 || k < 1 { return None; }
    let inline = extract_inline_percent(raw).unwrap_or(0) as f32;
    Some(((k - 1) as f32 + inline / 100.0) / n as f32)
}

/// Flatpak's live bar: `Installing… ███▌   45%  772.3 kB/s`, or `Installing 2/3… ...` for several refs.
/// The line is redrawn with `\r`, so only the last non-empty segment is current.
fn parse_flatpak_progress(raw: &str) -> Option<(&'static str, i32)> {
    let segment = raw.rsplit('\r').find(|s| !s.trim().is_empty())?;
    let lower = segment.to_lowercase();
    let label = if lower.starts_with("installing") {
        "Installing..."
    } else if lower.starts_with("updating") {
        "Updating..."
    } else if lower.starts_with("uninstalling") {
        "Removing..."
    } else {
        return None;
    };
    let pct = extract_inline_percent(segment)?;
    let step = lower
        .split_whitespace()
        .nth(1)
        .and_then(|t| t.trim_end_matches('…').split_once('/'))
        .and_then(|(k, n)| Some((k.parse::<i32>().ok()?, n.parse::<i32>().ok()?)))
        .filter(|&(k, n)| k >= 1 && n >= k);
    let total = match step {
        Some((k, n)) => ((k - 1) * 100 + pct) / n,
        None => pct,
    };
    Some((label, total))
}

/// Stage detection for non-pacman output (flatpak, AppImage). Percent is 0..=100.
fn detect_generic_phase(lower: &str, raw: &str) -> Option<(&'static str, i32)> {
    if let Some(p) = extract_done_percent(lower) {
        return Some(("Downloading...", 5 + (p * 0.85) as i32));
    }
    if (lower.contains("installing") || lower.contains("updating") || lower.contains("uninstalling"))
        && let Some(frac) = parse_bracket_fraction(raw)
    {
        let label = if lower.contains("uninstalling") { "Removing..." } else { "Installing..." };
        return Some((label, (5.0 + frac * 90.0) as i32));
    }
    if lower.contains("resolving") { return Some(("Resolving release...", 3)); }
    if lower.contains("downloading") || lower.contains("re-downloading") { return Some(("Downloading...", 8)); }
    if lower.contains("copying") || lower.contains("re-copying") { return Some(("Copying file...", 40)); }
    if lower.contains("updating via") { return Some(("Updating...", 5)); }
    if lower.contains("checking update support") { return Some(("Checking update support...", 88)); }
    if lower.contains("integrating") { return Some(("Integrating with menu...", 94)); }
    if lower.contains("removing") { return Some(("Removing...", 50)); }
    None
}

struct Inner {
    stream: TermStream,
    percent: i32,
    lo: i32,
    hi: i32,
    first_error: Option<String>,
    dirty: bool,
    captured: String,
}

impl Inner {
    fn mapped(&self) -> i32 {
        self.lo + self.percent * (self.hi - self.lo) / 100
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Mode {
    pub cancellable: bool,
}

/// Cloneable, thread-safe handle that feeds raw operation output into the progress popup.
#[derive(Clone)]
pub(crate) struct Feeder {
    tx: mpsc::Sender<UiMessage>,
    inner: Arc<Mutex<Inner>>,
    total_packages: usize,
}

fn flush(tx: &mpsc::Sender<UiMessage>, inner: &mut Inner) {
    if inner.dirty {
        let _ = tx.send(UiMessage::ProgressOutput(inner.stream.render()));
        inner.dirty = false;
    }
}

impl Feeder {
    /// Process one chunk of raw output; returns the cleaned text.
    pub(crate) fn feed(&self, bytes: &[u8]) -> String {
        let text = String::from_utf8_lossy(bytes);
        let cleaned = normalize_typographic(&strip_ansi(&text));
        let force_flush = handle_pty_prompt(&cleaned, &self.tx);

        let mut inner = self.inner.lock().unwrap();
        inner.stream.process(bytes);
        if inner.captured.len() < CAPTURE_LIMIT {
            inner.captured.push_str(&cleaned);
        }
        for line in cleaned.lines() {
            let lower = line.to_lowercase();
            if classify_log_level(&lower) == 1 && inner.first_error.is_none() {
                inner.first_error = Some(line.to_string());
            }
            let phase = parse_flatpak_progress(line)
                .or_else(|| detect_phase(&lower, line, self.total_packages))
                .or_else(|| detect_generic_phase(&lower, line));
            if let Some((label, pct)) = phase {
                if pct > inner.percent {
                    inner.percent = pct.min(100);
                }
                let _ = self.tx.send(UiMessage::OperationProgress(inner.mapped(), label.to_string()));
            }
        }
        inner.dirty = true;
        if force_flush {
            flush(&self.tx, &mut inner);
        }
        cleaned
    }

    pub(crate) fn line(&self, text: &str) {
        self.feed(format!("{text}\n").as_bytes());
    }

    /// Restrict reported percent to `lo..=hi` and restart the raw counter (multi-stage operations).
    pub(crate) fn window(&self, lo: i32, hi: i32) {
        let mut inner = self.inner.lock().unwrap();
        inner.lo = lo;
        inner.hi = hi;
        inner.percent = 0;
        let _ = self.tx.send(UiMessage::OperationProgress(lo, String::new()));
    }
}

/// Owns one popup session: opens it, throttles output flushes, closes it with the result.
pub(crate) struct OpReporter {
    feeder: Feeder,
    stop: Arc<AtomicBool>,
    flusher: Option<thread::JoinHandle<()>>,
}

impl OpReporter {
    pub(crate) fn start(
        tx: &mpsc::Sender<UiMessage>,
        title: &str,
        mode: Mode,
        total_packages: usize,
    ) -> Self {
        CANCEL_REQUESTED.store(false, Ordering::SeqCst);
        let _ = tx.send(UiMessage::ShowProgressPopup(title.to_string()));
        let _ = tx.send(UiMessage::ProgressMode { cancellable: mode.cancellable });
        let _ = tx.send(UiMessage::ProgressAutoExpand);

        let inner = Arc::new(Mutex::new(Inner {
            stream: TermStream::new(),
            percent: 0,
            lo: 0,
            hi: 100,
            first_error: None,
            dirty: false,
            captured: String::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let flusher = {
            let (tx, inner, stop) = (tx.clone(), inner.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(FLUSH_INTERVAL);
                    flush(&tx, &mut inner.lock().unwrap());
                }
            })
        };
        Self {
            feeder: Feeder { tx: tx.clone(), inner, total_packages: total_packages.max(1) },
            stop,
            flusher: Some(flusher),
        }
    }

    pub(crate) fn feeder(&self) -> Feeder {
        self.feeder.clone()
    }

    pub(crate) fn captured(&self) -> String {
        self.feeder.inner.lock().unwrap().captured.clone()
    }

    /// Stop the flusher and push any pending output.
    pub(crate) fn settle(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.flusher.take() {
            let _ = handle.join();
        }
        flush(&self.feeder.tx, &mut self.feeder.inner.lock().unwrap());
    }

    pub(crate) fn finish(mut self, success: bool) {
        self.settle();
        if !success {
            if let Some(err) = self.feeder.inner.lock().unwrap().first_error.clone() {
                let _ = self.feeder.tx.send(UiMessage::ProgressErrorSummary(err));
            }
            let _ = self.feeder.tx.send(UiMessage::ProgressAutoExpand);
        }
        let _ = self.feeder.tx.send(UiMessage::OperationDone(success));
    }
}

impl Drop for OpReporter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Run `cmd` in a PTY, streaming its output into `feeder`. `Ok(success)` once the child exits.
pub(crate) fn run_pty(
    feeder: &Feeder,
    cmd: &str,
    args: &[&str],
    input_sender: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pid_holder: &Arc<Mutex<Option<u32>>>,
) -> Result<bool, String> {
    use std::io::{Read, Write};
    use std::os::unix::io::FromRawFd;

    let (master_fd, child_pid) = crate::spawn_in_pty(cmd, args)?;
    *pid_holder.lock().unwrap() = Some(child_pid);

    let (in_tx, in_rx) = mpsc::channel::<String>();
    *input_sender.lock().unwrap() = Some(in_tx);

    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let (feeder, done) = (feeder.clone(), done.clone());
        thread::spawn(move || {
            let mut file = unsafe { std::fs::File::from_raw_fd(master_fd) };
            let mut buf = [0u8; 4096];
            loop {
                let ready = unsafe {
                    let mut pfd = libc::pollfd { fd: master_fd, events: libc::POLLIN, revents: 0 };
                    libc::poll(&mut pfd as *mut libc::pollfd, 1, 20)
                };
                if ready < 0 { break; }
                if ready == 0 {
                    if done.load(Ordering::Relaxed) { break; }
                    continue;
                }
                match file.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => { feeder.feed(&buf[..n]); }
                }
            }
            // The fd is closed by the caller after waitpid.
            std::mem::forget(file);
        })
    };

    let writer = thread::spawn(move || {
        let dup_fd = unsafe { libc::dup(master_fd) };
        if dup_fd < 0 { return; }
        let mut file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        while let Ok(input) = in_rx.recv() {
            if file.write_all(format!("{input}\n").as_bytes()).is_err() { break; }
            let _ = file.flush();
        }
    });

    let status = unsafe {
        let mut wstatus: libc::c_int = 0;
        libc::waitpid(child_pid as libc::pid_t, &mut wstatus, 0);
        wstatus
    };
    let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;

    *pid_holder.lock().unwrap() = None;
    *input_sender.lock().unwrap() = None;
    done.store(true, Ordering::Relaxed);
    unsafe { libc::close(master_fd); }
    let _ = reader.join();
    let _ = writer.join();
    Ok(success)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_percent_takes_last_token() {
        assert_eq!(extract_done_percent("1.97% done (1.24 of 62.89 mib)..."), Some(1.97));
        assert_eq!(extract_done_percent("0% done 1.5% done 42.50% done"), Some(42.5));
        assert_eq!(extract_done_percent("no progress here"), None);
    }

    #[test]
    fn generic_phase_maps_download_percent() {
        let (label, pct) = detect_generic_phase("50% done", "50% done").unwrap();
        assert_eq!(label, "Downloading...");
        assert_eq!(pct, 5 + 42);
    }

    #[test]
    fn generic_phase_reads_flatpak_counter() {
        let line = "[2/4] Installing org.example.App";
        let (label, pct) = detect_generic_phase(&line.to_lowercase(), line).unwrap();
        assert_eq!(label, "Installing...");
        assert_eq!(pct, 5 + 22);
    }

    #[test]
    fn flatpak_live_bar_reports_percent() {
        let raw = "\rInstalling\u{2026} \u{2588}\u{2588}\u{258c}                  18%  339.7\u{a0}kB/s\rInstalling\u{2026} \u{2588}\u{2588}\u{2588}                  25%  536.4\u{a0}kB/s";
        assert_eq!(parse_flatpak_progress(raw), Some(("Installing...", 25)));
    }

    #[test]
    fn flatpak_multi_ref_bar_scales_by_step() {
        let raw = "\rInstalling 2/4\u{2026} \u{2588}\u{2588}                  50%  1.0\u{a0}MB/s";
        assert_eq!(parse_flatpak_progress(raw), Some(("Installing...", 37)));
        assert_eq!(parse_flatpak_progress("\rUninstalling\u{2026}   100%"), Some(("Removing...", 100)));
        assert_eq!(parse_flatpak_progress("Looking for matches\u{2026}"), None);
    }

    #[test]
    fn osc_progress_sequences_leave_no_stray_characters() {
        let mut stream = TermStream::new();
        stream.process(b"\rInstalling\xe2\x80\xa6  50%\x1b]9;4;1;50\x1b\\\x1b[6n\r\n");
        assert_eq!(stream.render().trim(), "Installing...  50%");
    }

    #[test]
    fn flatpak_bar_redraws_collapse_to_one_log_line() {
        let mut stream = TermStream::new();
        stream.process("Looking for matches\u{2026}\r\n\rInstalling\u{2026}   10%\rInstalling\u{2026}   55%\r\n".as_bytes());
        let text = stream.render();
        assert!(text.contains("55%") && !text.contains("10%"), "{text:?}");
    }

    #[test]
    fn appimage_stages_are_recognised() {
        assert_eq!(detect_generic_phase("resolving latest release for x…", "").unwrap().0, "Resolving release...");
        assert_eq!(detect_generic_phase("integrating into application menu…", "").unwrap().0, "Integrating with menu...");
        assert!(detect_generic_phase("something unrelated", "").is_none());
    }

    #[test]
    fn pacman_progress_wins_over_generic() {
        let line = "(1/2) installing foo";
        let (label, _) = detect_phase(&line.to_lowercase(), line, 2).unwrap();
        assert_eq!(label, "Installing packages...");
    }

    #[test]
    fn feeder_reports_monotonic_percent_in_window() {
        let (tx, rx) = mpsc::channel();
        let rep = OpReporter::start(&tx, "t", Mode { cancellable: false }, 1);
        let f = rep.feeder();
        f.window(50, 100);
        f.feed(b"50% done\n");
        f.feed(b"10% done\n");
        rep.finish(true);
        let pcts: Vec<i32> = rx
            .try_iter()
            .filter_map(|m| match m {
                UiMessage::OperationProgress(p, l) if !l.is_empty() => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(pcts, vec![73, 73]);
    }

    #[test]
    fn failure_surfaces_first_error_and_expands() {
        let (tx, rx) = mpsc::channel();
        let rep = OpReporter::start(&tx, "t", Mode { cancellable: false }, 1);
        rep.feeder().feed(b"error: first problem\nerror: second problem\n");
        rep.finish(false);
        let msgs: Vec<UiMessage> = rx.try_iter().collect();
        assert!(msgs.iter().any(|m| matches!(m, UiMessage::ProgressErrorSummary(s) if s == "error: first problem")));
        assert!(msgs.iter().any(|m| matches!(m, UiMessage::ProgressAutoExpand)));
        assert!(matches!(msgs.last(), Some(UiMessage::OperationDone(false))));
    }

    #[test]
    fn yes_no_prompt_shows_buttons_with_real_text() {
        let (tx, rx) = mpsc::channel();
        assert!(handle_pty_prompt(":: Replace foo with bar? [Y/n]", &tx));
        let msgs: Vec<UiMessage> = rx.try_iter().collect();
        assert!(matches!(msgs[0], UiMessage::ProgressPromptButtons));
        assert!(matches!(&msgs[1], UiMessage::ProgressPrompt(t) if t == ":: Replace foo with bar? [Y/n]"));
    }
}


