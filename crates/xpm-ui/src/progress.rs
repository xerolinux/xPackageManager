use crate::UiMessage;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);
static TERM_COLS: AtomicUsize = AtomicUsize::new(80);

/// Width of the details log in monospace cells; the PTY and the emulator follow it so no line is cut off.
pub(crate) fn term_cols() -> usize {
    TERM_COLS.load(Ordering::Relaxed)
}

pub(crate) fn set_term_cols(cols: usize) {
    TERM_COLS.store(cols.clamp(20, 400), Ordering::Relaxed);
}

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

const TERM_ROWS: usize = 40;
const TERM_SCROLLBACK: usize = 4_000;

/// Terminal emulator behind the details log: cursor movement, line erasing and carriage
/// returns are interpreted, so the text matches what a real terminal would show.
struct TermStream {
    term: avt::Terminal,
}

impl TermStream {
    fn new() -> Self {
        let term = avt::Terminal::with_scrollback(term_cols(), TERM_ROWS, Some(TERM_SCROLLBACK))
            .expect("terminal size is non-zero");
        Self { term }
    }

    fn process(&mut self, bytes: &[u8]) {
        let cols = term_cols();
        if self.term.size().0 != cols {
            self.term.resize(cols, TERM_ROWS);
        }
        self.term.feed(String::from_utf8_lossy(bytes).as_ref());
    }

    /// Visible and scrollback rows as styled runs; every row fits the terminal width.
    fn lines(&self) -> Vec<StyledLine> {
        let mut rows: Vec<StyledLine> = self.term.screen().all_lines().map(row_to_runs).collect();
        while rows.last().is_some_and(|r| r.iter().all(|run| run.text.trim().is_empty())) {
            rows.pop();
        }
        rows
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StyledRun {
    pub text: String,
    /// ANSI palette index 0..=15, or -1 for the default colour.
    pub ansi: i32,
    pub rgb: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub faint: bool,
}

pub(crate) type StyledLine = Vec<StyledRun>;

fn palette_256(i: u8) -> (u8, u8, u8) {
    const STEP: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match i {
        16..=231 => {
            let n = i - 16;
            (STEP[(n / 36) as usize], STEP[((n / 6) % 6) as usize], STEP[(n % 6) as usize])
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

fn row_to_runs(line: &avt::Line) -> StyledLine {
    let mut runs: StyledLine = Vec::new();
    for cell in line.cells() {
        if cell.width() == 0 {
            continue;
        }
        let pen = cell.pen();
        let (ansi, rgb) = match pen.foreground() {
            None => (-1, None),
            Some(avt::Color::Indexed(i)) if i < 16 => (i as i32, None),
            Some(avt::Color::Indexed(i)) => (-1, Some(palette_256(i))),
            Some(avt::Color::RGB(c)) => (-1, Some((c.r, c.g, c.b))),
        };
        let (bold, faint) = (pen.is_bold(), pen.is_faint());
        let ch = cell.char();
        match runs.last_mut() {
            Some(r) if r.ansi == ansi && r.rgb == rgb && r.bold == bold && r.faint == faint => r.text.push(ch),
            _ => runs.push(StyledRun { text: ch.to_string(), ansi, rgb, bold, faint }),
        }
    }
    if let Some(last) = runs.last_mut() {
        let trimmed = last.text.trim_end().len();
        last.text.truncate(trimmed);
    }
    while runs.last().is_some_and(|r| r.text.is_empty()) {
        runs.pop();
    }
    for r in &mut runs {
        r.text = normalize_typographic(&r.text);
    }
    runs
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
fn is_proceed_prompt(cleaned: &str) -> bool {
    cleaned.contains("Proceed with installation? [Y/n]") || cleaned.contains("Proceed with download? [Y/n]")
}

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

pub(crate) const PACMAN_PHASES: [&str; 6] = ["Sync", "Resolve", "Download", "Verify", "Install", "Hooks"];

fn pacman_phase(lower: &str, active: usize) -> Option<usize> {
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if has(&["post-transaction"]) {
        Some(5)
    } else if parse_action(lower.trim()).is_some() {
        Some(4)
    } else if has(&["checking keys", "checking keyring", "checking integrity", "checking package integrity", "loading package files", "checking for file conflicts", "checking available disk space", "pre-transaction"]) {
        Some(3)
    } else if has(&["retrieving packages", "total download size"]) || (active >= 1 && has(&["mib/s", "kib/s"])) {
        Some(2)
    } else if has(&["resolving dependencies", "looking for conflicting"]) {
        Some(1)
    } else if has(&["synchronizing package databases"]) {
        Some(0)
    } else {
        None
    }
}

/// Moves the pacman stage indicator forward as output lines arrive.
#[derive(Default)]
struct PhaseTracker {
    enabled: bool,
    line: String,
    active: usize,
    saw_sync: bool,
}

impl PhaseTracker {
    /// True while pacman is refreshing its databases (their transfer lines are not package downloads).
    fn syncing(&self) -> bool {
        self.saw_sync && self.active == 0
    }

    fn feed(&mut self, text: &str, tx: &mpsc::Sender<UiMessage>) {
        for ch in text.chars() {
            match ch {
                '\n' | '\r' => {
                    self.check(tx);
                    self.line.clear();
                }
                c => self.line.push(c),
            }
        }
        self.check(tx);
    }

    fn check(&mut self, tx: &mpsc::Sender<UiMessage>) {
        let Some(p) = pacman_phase(&self.line.to_lowercase(), self.active) else { return };
        self.saw_sync |= p == 0;
        if p > self.active {
            self.active = p;
            if self.enabled {
                let _ = tx.send(UiMessage::ProgressPhase(p));
            }
        }
    }
}

pub(crate) struct RowInfo {
    pub name: String,
    pub tag: &'static str,
    pub detail: String,
}

/// Follows pacman output and reports one progress row per package.
#[derive(Default)]
struct PacmanRows {
    /// Text of the current output line, and the part after its latest carriage return.
    first: String,
    last: String,
    seen_cr: bool,
    scan: Scan,
    pending: Vec<RowInfo>,
    /// Absolute row index of the first planned package, and package names in plan order.
    base: usize,
    names: Vec<String>,
    active: Option<usize>,
    last_pct: i32,
}

#[derive(Default, PartialEq, Eq)]
enum Scan {
    #[default]
    Idle,
    Table,
    List,
}

fn package_name_from_nvr(token: &str) -> String {
    let mut parts = token.rsplitn(3, '-');
    let (_rel, _ver, name) = (parts.next(), parts.next(), parts.next());
    name.unwrap_or(token).to_string()
}

fn parse_action(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('(')?;
    let (counter, rest) = rest.split_once(") ")?;
    let (k, n) = counter.split_once('/')?;
    k.parse::<u32>().ok()?;
    n.parse::<u32>().ok()?;
    let mut it = rest.split_whitespace();
    let verb = it.next()?;
    matches!(verb, "upgrading" | "installing" | "removing" | "reinstalling" | "downgrading").then_some(())?;
    Some((verb, it.next()?))
}

impl PacmanRows {
    fn feed(&mut self, text: &str, rows: &mut RowSink<'_>) {
        for ch in text.chars() {
            match ch {
                '\n' => {
                    let line = std::mem::take(&mut self.first);
                    self.last.clear();
                    self.seen_cr = false;
                    self.finish_line(&line, rows);
                }
                '\r' => {
                    let line = self.first.clone();
                    self.track_progress(&line, rows);
                    self.seen_cr = true;
                }
                c => {
                    if self.seen_cr {
                        self.seen_cr = false;
                        self.last.clear();
                    }
                    self.first.push(c);
                    self.last.push(c);
                }
            }
        }
        let line = self.first.clone();
        self.track_progress(&line, rows);
    }

    fn finish_line(&mut self, line: &str, rows: &mut RowSink<'_>) {
        let trimmed = line.trim();
        if self.scan != Scan::Idle {
            if trimmed.is_empty() {
                if !(self.scan == Scan::Table && self.pending.is_empty()) {
                    self.flush_plan(rows);
                }
            } else if self.scan == Scan::Table {
                let mut cols = trimmed.split_whitespace();
                if let Some(first) = cols.next() {
                    let name = first.rsplit('/').next().unwrap_or(first).to_string();
                    let vers: Vec<&str> = cols.take(2).collect();
                    let detail = match vers.as_slice() {
                        [old, new] if new.chars().any(|c| c.is_ascii_digit()) => format!("{old} \u{2192} {new}"),
                        [one] => (*one).to_string(),
                        _ => String::new(),
                    };
                    self.pending.push(RowInfo { name, tag: "", detail });
                }
            } else {
                for tok in trimmed.split_whitespace() {
                    self.pending.push(RowInfo { name: package_name_from_nvr(tok), tag: "", detail: String::new() });
                }
            }
            return;
        }
        if let Some(rest) = trimmed.strip_prefix("Packages (").or_else(|| trimmed.strip_prefix("Package (")) {
            self.pending.clear();
            if let Some((_, tail)) = rest.split_once(')') {
                if tail.contains("Old Version") {
                    self.scan = Scan::Table;
                } else {
                    self.scan = Scan::List;
                    for tok in tail.split_whitespace() {
                        self.pending.push(RowInfo { name: package_name_from_nvr(tok), tag: "", detail: String::new() });
                    }
                }
            }
            return;
        }
        if let Some((_, name)) = parse_action(trimmed) {
            self.activate(name, rows);
        }
    }

    fn flush_plan(&mut self, rows: &mut RowSink<'_>) {
        self.scan = Scan::Idle;
        if self.pending.is_empty() {
            return;
        }
        self.names = self.pending.iter().map(|r| r.name.clone()).collect();
        self.base = rows.append(std::mem::take(&mut self.pending));
        self.active = None;
    }

    fn activate(&mut self, name: &str, rows: &mut RowSink<'_>) {
        let Some(i) = self.names.iter().position(|n| n == name) else { return };
        if self.active == Some(i) {
            return;
        }
        if let Some(prev) = self.active {
            rows.state(self.base + prev, 2, 100);
        }
        self.active = Some(i);
        self.last_pct = 0;
        rows.state(self.base + i, 1, 0);
    }

    fn track_progress(&mut self, line: &str, rows: &mut RowSink<'_>) {
        let Some((_, name)) = parse_action(line.trim()) else { return };
        self.activate(name, rows);
        if let Some(i) = self.active
            && let Some(p) = extract_inline_percent(&self.last)
            && p != self.last_pct
        {
            self.last_pct = p;
            rows.state(self.base + i, 1, p);
        }
    }

    fn complete(&mut self, rows: &mut RowSink<'_>) {
        for i in 0..self.names.len() {
            rows.state(self.base + i, 2, 100);
        }
    }
}

/// Where tracked rows go: the UI channel plus a running count so rows from several stages stack.
struct RowSink<'a> {
    tx: &'a mpsc::Sender<UiMessage>,
    len: &'a mut usize,
}

impl RowSink<'_> {
    fn append(&mut self, rows: Vec<RowInfo>) -> usize {
        let first = *self.len;
        *self.len += rows.len();
        let _ = self.tx.send(UiMessage::ProgressRowsAppend { first, rows });
        first
    }

    fn state(&self, index: usize, state: i32, percent: i32) {
        let _ = self.tx.send(UiMessage::ProgressRowState { index, state, percent });
    }
}

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct Counts {
    pub upgraded: i32,
    pub installed: i32,
    pub removed: i32,
}

impl Counts {
    fn add(&mut self, o: Counts) {
        self.upgraded += o.upgraded;
        self.installed += o.installed;
        self.removed += o.removed;
    }

    fn total(&self) -> i32 {
        self.upgraded + self.installed + self.removed
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Format {
    Flatpak,
    AppImage,
}

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct Summary {
    pub native: Counts,
    pub flatpak: Counts,
    pub appimage: Counts,
    pub warnings: i32,
}

/// One result pill: `kind` 0 upgraded, 1 installed, 2 removed, 3 warnings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SummaryTile {
    pub label: String,
    pub count: i32,
    pub kind: i32,
}

impl Summary {
    fn add(&mut self, o: Summary) {
        self.native.add(o.native);
        self.flatpak.add(o.flatpak);
        self.appimage.add(o.appimage);
        self.warnings += o.warnings;
    }

    fn counts_mut(&mut self, format: Format) -> &mut Counts {
        match format {
            Format::Flatpak => &mut self.flatpak,
            Format::AppImage => &mut self.appimage,
        }
    }

    fn changed(&self) -> i32 {
        self.native.total() + self.flatpak.total() + self.appimage.total()
    }

    /// One pill per package format and kind of change, plus one for serious warnings.
    fn tiles(&self) -> Vec<SummaryTile> {
        let mut tiles = Vec::new();
        for (name, c) in [("Native", self.native), ("Flatpak", self.flatpak), ("AppImage", self.appimage)] {
            for (verb, count, kind) in [("upgraded", c.upgraded, 0), ("installed", c.installed, 1), ("removed", c.removed, 2)] {
                if count > 0 {
                    tiles.push(SummaryTile { label: format!("{name} {verb}"), count, kind });
                }
            }
        }
        if self.warnings > 0 {
            tiles.push(SummaryTile { label: "Warnings".to_string(), count: self.warnings, kind: 3 });
        }
        tiles
    }
}

/// Warnings worth interrupting the user for; routine ones (.pacnew files, reinstalls) stay in the log.
fn is_important_warning(line: &str) -> bool {
    const SERIOUS: [&str; 11] = [
        "failed", "corrupt", "invalid", "could not", "cannot", "unable", "conflict", "signature", "keyring",
        "no space", "end-of-life",
    ];
    let lower = line.to_lowercase();
    SERIOUS.iter().any(|w| lower.contains(w))
}

/// Counts what a finished pacman or flatpak run changed, from its output.
fn summarize(output: &str) -> Summary {
    let mut sum = Summary::default();
    let mut seen = std::collections::HashSet::new();
    for seg in output.split(['\n', '\r']) {
        let line = seg.trim();
        if let Some(rest) = line.strip_prefix('(')
            && let Some((counter, action)) = rest.split_once(") ")
            && counter.split_once('/').is_some_and(|(k, n)| k.parse::<u32>().is_ok() && n.parse::<u32>().is_ok())
        {
            let verb = action.split_whitespace().next().unwrap_or_default();
            if seen.insert(format!("{counter} {verb}")) {
                match verb {
                    "upgrading" | "downgrading" => sum.native.upgraded += 1,
                    "installing" | "reinstalling" => sum.native.installed += 1,
                    "removing" => sum.native.removed += 1,
                    _ => {}
                }
            }
            continue;
        }
        let mut cols = line.split_whitespace();
        if cols.next().is_some_and(|c| c.ends_with('.') && c[..c.len() - 1].parse::<u32>().is_ok()) {
            let op = line.split_whitespace().nth(3);
            if seen.insert(format!("flatpak {line}")) {
                match op {
                    Some("u") => sum.flatpak.upgraded += 1,
                    Some("i") => sum.flatpak.installed += 1,
                    Some("r") => sum.flatpak.removed += 1,
                    _ => {}
                }
            }
            continue;
        }
        if line.starts_with("warning:") && is_important_warning(line) && seen.insert(line.to_string()) {
            sum.warnings += 1;
        }
    }
    sum
}

/// Length of the prefix of `buf` that ends on a character and escape-sequence boundary.
/// PTY reads can cut a multibyte character or an escape sequence in half.
fn complete_len(buf: &[u8]) -> usize {
    let n = buf.len();
    let mut end = n;
    for back in 1..=3.min(n) {
        let b = buf[n - back];
        if b & 0xc0 == 0x80 { continue; }
        let need = match b { 0xc0..=0xdf => 2, 0xe0..=0xef => 3, 0xf0..=0xf7 => 4, _ => 1 };
        if need > back { end = n - back; }
        break;
    }
    const MAX_ESC: usize = 256;
    if let Some(pos) = buf[..end].iter().rposition(|&b| b == 0x1b).filter(|&p| end - p < MAX_ESC) {
        let seq = &buf[pos + 1..end];
        let open = match seq.first() {
            None => true,
            Some(b'[') => !seq[1..].iter().any(|b| (0x40..=0x7e).contains(b)),
            Some(b']') => !(seq.contains(&0x07) || seq.windows(2).any(|w| w == [0x1b, b'\\'])),
            Some(b'(' | b')' | b'*' | b'+') => seq.len() < 2,
            Some(_) => false,
        };
        if open { end = pos; }
    }
    end
}

struct Inner {
    carry: Vec<u8>,
    stream: TermStream,
    percent: i32,
    lo: i32,
    hi: i32,
    first_error: Option<String>,
    dirty: bool,
    captured: String,
    extra: Summary,
    rows_len: usize,
    pacman_rows: PacmanRows,
    /// Row that mirrors the overall percent (single-item backends such as AppImage).
    single_row: Option<usize>,
    percent_hook: Option<Arc<dyn Fn(i32) + Send + Sync>>,
    phases: PhaseTracker,
    /// Answer pacman's "Proceed with installation?" automatically (the user already said yes globally).
    auto_yes: bool,
    input: Option<Arc<Mutex<Option<mpsc::Sender<String>>>>>,
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
        let _ = tx.send(UiMessage::ProgressLines(inner.stream.lines()));
        inner.dirty = false;
    }
}

impl Feeder {
    /// Process one chunk of raw output; returns the cleaned text.
    pub(crate) fn feed(&self, bytes: &[u8]) -> String {
        let mut inner = self.inner.lock().unwrap();
        let mut data = std::mem::take(&mut inner.carry);
        data.extend_from_slice(bytes);
        let end = complete_len(&data);
        inner.carry = data.split_off(end);
        let bytes = data.as_slice();

        let text = String::from_utf8_lossy(bytes);
        let cleaned = normalize_typographic(&strip_ansi(&text));
        let force_flush = if inner.auto_yes && is_proceed_prompt(&cleaned) {
            if let Some(input) = &inner.input
                && let Some(sender) = input.lock().unwrap().as_ref()
            {
                let _ = sender.send("y".to_string());
            }
            false
        } else {
            handle_pty_prompt(&cleaned, &self.tx)
        };

        inner.stream.process(bytes);
        {
            let Inner { pacman_rows, rows_len, .. } = &mut *inner;
            pacman_rows.feed(&cleaned, &mut RowSink { tx: &self.tx, len: rows_len });
        }
        inner.phases.feed(&cleaned, &self.tx);
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
            if let Some((mut label, pct)) = phase {
                if label == "Downloading packages..." && inner.phases.syncing() {
                    label = "Synchronizing databases...";
                }
                if pct > inner.percent {
                    inner.percent = pct.min(100);
                }
                let _ = self.tx.send(UiMessage::OperationProgress(inner.mapped(), label.to_string()));
                if let Some(hook) = &inner.percent_hook {
                    hook(inner.mapped());
                }
                if let Some(row) = inner.single_row {
                    let _ = self.tx.send(UiMessage::ProgressRowState { index: row, state: 1, percent: inner.percent });
                }
            }
        }
        inner.dirty = true;
        if force_flush {
            flush(&self.tx, &mut inner);
        }
        cleaned
    }

    /// Appends a line the way a PTY would deliver it (`\r\n`), so the emulator returns to column 0.
    pub(crate) fn line(&self, text: &str) {
        self.feed(format!("{}\r\n", text.replace('\n', "\r\n")).as_bytes());
    }

    pub(crate) fn report(&self, percent: i32, stage: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.percent = inner.percent.max(percent.clamp(0, 100));
        let _ = self.tx.send(UiMessage::OperationProgress(inner.mapped(), stage.to_string()));
    }

    /// Appends progress rows; returns the absolute index of the first one.
    pub(crate) fn rows_append(&self, rows: Vec<RowInfo>) -> usize {
        let mut inner = self.inner.lock().unwrap();
        RowSink { tx: &self.tx, len: &mut inner.rows_len }.append(rows)
    }

    pub(crate) fn set_auto_yes(&self, on: bool) {
        self.inner.lock().unwrap().auto_yes = on;
    }

    pub(crate) fn bind_input(&self, input: Arc<Mutex<Option<mpsc::Sender<String>>>>) {
        self.inner.lock().unwrap().input = Some(input);
    }

    /// Shows the pacman stage indicator (sync, resolve, download, verify, install, hooks).
    pub(crate) fn enable_phases(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.phases.enabled = true;
        inner.phases.active = 0;
        let _ = self.tx.send(UiMessage::ProgressPhases(PACMAN_PHASES.iter().map(|p| p.to_string()).collect()));
    }

    /// Calls `hook` with the overall percent whenever it advances.
    pub(crate) fn on_percent(&self, hook: impl Fn(i32) + Send + Sync + 'static) {
        self.inner.lock().unwrap().percent_hook = Some(Arc::new(hook));
    }

    pub(crate) fn lines(&self) -> Vec<StyledLine> {
        self.inner.lock().unwrap().stream.lines()
    }

    /// Makes `index` follow the percent parsed from the output until the next call.
    pub(crate) fn track_single(&self, index: Option<usize>) {
        let mut inner = self.inner.lock().unwrap();
        inner.single_row = index;
        inner.percent = 0;
    }

    pub(crate) fn row_state(&self, index: usize, state: i32, percent: i32) {
        let _ = self.tx.send(UiMessage::ProgressRowState { index, state, percent });
    }

    /// Counts from a typed backend, added to whatever is parsed from the output.
    pub(crate) fn add_summary(&self, format: Format, counts: Counts) {
        self.inner.lock().unwrap().extra.counts_mut(format).add(counts);
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

        let inner = Arc::new(Mutex::new(Inner {
            carry: Vec::new(),
            stream: TermStream::new(),
            percent: 0,
            lo: 0,
            hi: 100,
            first_error: None,
            dirty: false,
            captured: String::new(),
            extra: Summary::default(),
            rows_len: 0,
            pacman_rows: PacmanRows::default(),
            single_row: None,
            percent_hook: None,
            phases: PhaseTracker::default(),
            auto_yes: false,
            input: None,
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
        if success {
            if self.feeder.inner.lock().unwrap().phases.enabled {
                let _ = self.feeder.tx.send(UiMessage::ProgressPhase(PACMAN_PHASES.len()));
            }
            {
                let mut inner = self.feeder.inner.lock().unwrap();
                let Inner { pacman_rows, rows_len, .. } = &mut *inner;
                pacman_rows.complete(&mut RowSink { tx: &self.feeder.tx, len: rows_len });
            }
            let inner = self.feeder.inner.lock().unwrap();
            let mut sum = summarize(&inner.captured);
            sum.add(inner.extra);
            drop(inner);
            if sum.changed() > 0 {
                let _ = self.feeder.tx.send(UiMessage::ProgressSummary(sum.tiles()));
            }
        }
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

    feeder.bind_input(input_sender.clone());
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
            let mut cols = term_cols();
            loop {
                if term_cols() != cols {
                    cols = term_cols();
                    let ws = libc::winsize { ws_col: cols as u16, ws_row: crate::PTY_ROWS, ws_xpixel: 0, ws_ypixel: 0 };
                    unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws); }
                }
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
        assert_eq!(plain(&stream.lines()).trim(), "Installing...  50%");
    }

    #[test]
    fn flatpak_bar_redraws_collapse_to_one_log_line() {
        let mut stream = TermStream::new();
        stream.process("Looking for matches\u{2026}\r\n\rInstalling\u{2026}   10%\rInstalling\u{2026}   55%\r\n".as_bytes());
        let text = plain(&stream.lines());
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

    fn plain(lines: &[StyledLine]) -> String {
        lines.iter().map(|l| l.iter().map(|r| r.text.as_str()).collect::<String>() + "\n").collect()
    }

    fn rendered(chunks: &[&[u8]]) -> String {
        let (tx, rx) = mpsc::channel();
        let rep = OpReporter::start(&tx, "t", Mode { cancellable: false }, 1);
        for c in chunks {
            rep.feeder().feed(c);
        }
        let captured = rep.captured();
        rep.finish(true);
        let out = rx
            .try_iter()
            .filter_map(|m| match m {
                UiMessage::ProgressLines(l) => Some(plain(&l)),
                _ => None,
            })
            .last()
            .unwrap_or_default();
        assert!(!captured.contains('\u{fffd}'), "captured has replacement chars: {captured:?}");
        out
    }

    #[test]
    fn multibyte_char_split_across_reads_survives() {
        let bar = "\u{2588}\u{2588}\u{258c}".as_bytes();
        let out = rendered(&[&bar[..1], &bar[1..4], &bar[4..], b" ok\n"]);
        assert_eq!(out.trim(), "\u{2588}\u{2588}\u{258c} ok");
    }

    #[test]
    fn escape_sequence_split_across_reads_leaves_no_stray_text() {
        let out = rendered(&[b"\x1b", b"[1;32mgreen\x1b[0", b"m done\n"]);
        assert_eq!(out.trim(), "green done");
    }

    #[test]
    fn osc_split_across_reads_leaves_no_stray_text() {
        let out = rendered(&[b"a\x1b]9;4;1;5", b"0\x1b", b"\\b\n"]);
        assert_eq!(out.trim(), "ab");
    }

    #[test]
    fn terminal_emulation_handles_cursor_movement_and_erase() {
        let mut stream = TermStream::new();
        stream.process(b"\x1b[?25l:: Synchronizing...\r\n core  50%\rcore 100%\x1b[K\r\n");
        stream.process(b"xerolinux is up to date\r\n:: Starting\r\n");
        stream.process(b"old line\r\n\x1b[1A\x1b[2Knew line\r\n\x1b[?25h");
        assert_eq!(
            plain(&stream.lines()),
            ":: Synchronizing...\ncore 100%\nxerolinux is up to date\n:: Starting\nnew line\n"
        );
    }

    #[test]
    fn terminal_keeps_scrollback_and_never_exceeds_the_width() {
        let mut stream = TermStream::new();
        let long = "x".repeat(200);
        stream.process(format!("{long}\r\n").as_bytes());
        for n in 0..100 {
            stream.process(format!("line {n}\r\n").as_bytes());
        }
        let rows = stream.lines();
        let cols = term_cols();
        assert!(rows.iter().all(|r| r.iter().map(|run| run.text.chars().count()).sum::<usize>() <= cols));
        assert_eq!(rows[0][0].text.len(), cols);
        let text = plain(&rows);
        assert!(text.contains("line 0\n") && text.ends_with("line 99\n"));
    }

    #[test]
    fn colours_and_bold_become_runs() {
        let mut stream = TermStream::new();
        stream.process(b"\x1b[1;32mgreen\x1b[0m plain \x1b[38;5;196mred\x1b[0m\r\n");
        let row = &stream.lines()[0];
        assert_eq!((row[0].text.as_str(), row[0].ansi, row[0].bold), ("green", 2, true));
        assert_eq!((row[1].text.as_str(), row[1].ansi), (" plain ", -1));
        assert_eq!(row[2].rgb, Some((255, 0, 0)));
    }

    #[test]
    fn summary_counts_pacman_actions_once_despite_redraws() {
        let out = "(1/3) upgrading 7zip   [#--]  10%\r(1/3) upgrading 7zip   [###] 100%\n(2/3) installing foo\n(3/3) removing bar\n(1/1) Arming ConditionNeedsUpdate...\nwarning: /etc/x installed as .pacnew\nwarning: failed to run hook\n";
        let sum = summarize(out);
        assert_eq!(sum.native, Counts { upgraded: 1, installed: 1, removed: 1 });
        assert_eq!(sum.warnings, 1, "only the serious warning counts");
    }

    #[test]
    fn summary_counts_flatpak_transaction_rows() {
        let out = " 1. org.gnome.Platform  49  u  flathub  < 300 MB\n 2. org.gimp.GIMP  stable  i  flathub  < 90 MB\n 3. org.old.App  stable  r  flathub\n";
        assert_eq!(summarize(out).flatpak, Counts { upgraded: 1, installed: 1, removed: 1 });
    }

    #[test]
    fn result_pills_are_split_per_package_format() {
        let mut sum = Summary::default();
        sum.native.upgraded = 3;
        sum.flatpak.upgraded = 2;
        sum.flatpak.installed = 1;
        sum.appimage.upgraded = 1;
        let tiles: Vec<(String, i32)> = sum.tiles().into_iter().map(|t| (t.label, t.count)).collect();
        assert_eq!(
            tiles,
            vec![
                ("Native upgraded".to_string(), 3),
                ("Flatpak upgraded".to_string(), 2),
                ("Flatpak installed".to_string(), 1),
                ("AppImage upgraded".to_string(), 1),
            ]
        );
    }

    #[test]
    fn proceed_prompts_are_answered_when_the_user_said_yes_globally() {
        assert!(is_proceed_prompt(":: Proceed with installation? [Y/n]"));
        assert!(!is_proceed_prompt(":: Replace foo with bar? [Y/n]"));
    }

    #[test]
    fn synthetic_lines_start_at_column_zero() {
        let (tx, rx) = mpsc::channel();
        let rep = OpReporter::start(&tx, "t", Mode { cancellable: false }, 1);
        rep.feeder().line("first line");
        rep.feeder().line("second");
        rep.finish(true);
        let last = rx.try_iter().filter_map(|m| match m { UiMessage::ProgressLines(l) => Some(plain(&l)), _ => None }).last().unwrap();
        assert_eq!(last, "first line\nsecond\n");
    }

    fn row_messages(chunks: &[&str]) -> Vec<String> {
        let (tx, rx) = mpsc::channel();
        let mut rows = PacmanRows::default();
        let mut len = 0;
        for c in chunks {
            rows.feed(c, &mut RowSink { tx: &tx, len: &mut len });
        }
        rx.try_iter()
            .map(|m| match m {
                UiMessage::ProgressRowsAppend { first, rows } => {
                    format!("plan {first}: {}", rows.iter().map(|r| format!("{} [{}]", r.name, r.detail)).collect::<Vec<_>>().join(", "))
                }
                UiMessage::ProgressRowState { index, state, percent } => format!("row {index} state {state} {percent}%"),
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn pacman_table_becomes_rows_and_tracks_progress() {
        let msgs = row_messages(&[
            "Packages (2)  Old Version  New Version  Net Change\r\n\r\n",
            "extra/7zip   26.04-1   26.05-1   0.00 MiB\r\ncore/glibc   2.41-2   2.42-1   1.20 MiB\r\n\r\n",
            "(1/2) upgrading 7zip   [##    ]  40%\r(1/2) upgrading 7zip   [#####] 100%\r\n",
            "(2/2) upgrading glibc  [#     ]  20%",
        ]);
        assert_eq!(msgs[0], "plan 0: 7zip [26.04-1 \u{2192} 26.05-1], glibc [2.41-2 \u{2192} 2.42-1]");
        assert!(msgs.contains(&"row 0 state 1 40%".to_string()), "{msgs:?}");
        assert!(msgs.contains(&"row 0 state 2 100%".to_string()), "{msgs:?}");
        assert!(msgs.contains(&"row 1 state 1 20%".to_string()), "{msgs:?}");
    }

    #[test]
    fn pacman_install_list_becomes_rows() {
        let msgs = row_messages(&["Packages (2) foo-1.2.3-1  bar-baz-4.5-2\r\n\r\n"]);
        assert_eq!(msgs[0], "plan 0: foo [], bar-baz []");
    }

    #[test]
    fn pacman_output_advances_the_stage_indicator() {
        let (tx, rx) = mpsc::channel();
        let mut t = PhaseTracker { enabled: true, ..PhaseTracker::default() };
        t.feed(" core  135.2 KiB  1234 KiB/s 00:00 [###] 100%\r\n", &tx);
        assert!(rx.try_recv().is_err(), "database transfers are not package downloads");
        for chunk in [
            ":: Synchronizing package databases...\r\n",
            "resolving dependencies...\r\n",
            "(2/2) checking package integrity\r\n",
            "(1/2) upgrading 7zip   [##]  10%\r",
            ":: Running post-transaction hooks...\r\n",
        ] {
            t.feed(chunk, &tx);
        }
        let seen: Vec<usize> = rx.try_iter().filter_map(|m| match m { UiMessage::ProgressPhase(p) => Some(p), _ => None }).collect();
        assert_eq!(seen, vec![1, 3, 4, 5]);
    }
}
