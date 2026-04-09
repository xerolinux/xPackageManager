use std::time::Instant;
use xpm_core::operation::{OperationProgress, OperationStatus};

#[derive(Debug)]
pub struct ProgressTracker {
    current: Option<TrackedOperation>,
}

#[derive(Debug)]
pub struct TrackedOperation {
    pub started_at: Instant,
    pub last_progress: OperationProgress,
    pub history: Vec<(Instant, u64)>,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self { current: None }
    }

    pub fn start(&mut self, total_packages: usize, total_bytes: u64) {
        self.current = Some(TrackedOperation {
            started_at: Instant::now(),
            last_progress: OperationProgress::new(total_packages, total_bytes),
            history: Vec::new(),
        });
    }

    // track download rate from recent samples
    pub fn update(&mut self, progress: OperationProgress) {
        if let Some(ref mut op) = self.current {
            if progress.status == OperationStatus::Downloading {
                op.history.push((Instant::now(), progress.downloaded_bytes));

                if op.history.len() > 10 {
                    op.history.remove(0);
                }
            }

            op.last_progress = progress;
        }
    }

    pub fn current(&self) -> Option<&OperationProgress> {
        self.current.as_ref().map(|op| &op.last_progress)
    }

    pub fn elapsed_secs(&self) -> Option<f64> {
        self.current
            .as_ref()
            .map(|op| op.started_at.elapsed().as_secs_f64())
    }

    pub fn download_speed(&self) -> Option<u64> {
        let op = self.current.as_ref()?;

        if op.history.len() < 2 {
            return None;
        }

        let first = op.history.first()?;
        let last = op.history.last()?;

        let bytes = last.1.saturating_sub(first.1);
        let secs = last.0.duration_since(first.0).as_secs_f64();

        if secs > 0.0 {
            Some((bytes as f64 / secs) as u64)
        } else {
            None
        }
    }

    // eta based on curent speed
    pub fn estimated_remaining(&self) -> Option<f64> {
        let op = self.current.as_ref()?;
        let speed = self.download_speed()?;

        if speed == 0 {
            return None;
        }

        let remaining_bytes = op
            .last_progress
            .total_bytes
            .saturating_sub(op.last_progress.downloaded_bytes);

        Some(remaining_bytes as f64 / speed as f64)
    }

    pub fn clear(&mut self) {
        self.current = None;
    }

    pub fn is_active(&self) -> bool {
        self.current.is_some()
    }
}

impl Default for ProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

pub fn format_duration(secs: f64) -> String {
    if secs < 60.0 {
        format!("{:.0}s", secs)
    } else if secs < 3600.0 {
        let mins = (secs / 60.0).floor();
        let secs = secs % 60.0;
        format!("{}m {:.0}s", mins, secs)
    } else {
        let hours = (secs / 3600.0).floor();
        let mins = ((secs % 3600.0) / 60.0).floor();
        format!("{}h {}m", hours, mins)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(1536), "1.50 KB");
        assert_eq!(format_bytes(1048576), "1.00 MB");
        assert_eq!(format_bytes(1073741824), "1.00 GB");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(30.0), "30s");
        assert_eq!(format_duration(90.0), "1m 30s");
        assert_eq!(format_duration(3661.0), "1h 1m");
    }
}
