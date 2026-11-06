//! Whole-system readings for the Home "System pulse" card: current values plus smooth scrolling traces.

use crate::live::{parse_stat, percent};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(crate) const METRICS: usize = 4;

/// Visible history, in samples. Traces carry one extra point so the graph can scroll between samples.
const POINTS: usize = 60;
const SAMPLE_EVERY: Duration = Duration::from_millis(200);

pub(crate) struct Fresh {
    pub values: [i32; METRICS],
    pub paths: [String; METRICS],
    /// Disk throughput, e.g. "12.4 MB/s"; the disk trace itself shows how busy the disk is.
    pub disk_rate: String,
}

pub(crate) struct Tick {
    /// Milliseconds until the next tick should fire.
    pub interval_ms: u32,
    /// How far the traces have scrolled towards the next sample, 0 to 1.
    pub phase: f32,
    /// Set when a new sample was taken on this tick.
    pub fresh: Option<Fresh>,
}

pub(crate) struct Sampler {
    prev: Option<(u64, u64)>,
    disks: Option<(Instant, Vec<DiskStat>)>,
    last: Instant,
    history: [VecDeque<f32>; METRICS],
    last_tick: Instant,
    interval_ms: u32,
    lag_ms: f32,
    changed: Instant,
}

/// Tick rates, fastest first. A busy UI thread steps down so the card never starves the window.
const RATES_MS: [u32; 3] = [16, 33, 50];

impl Default for Sampler {
    fn default() -> Self {
        Self {
            prev: None,
            disks: None,
            last: Instant::now(),
            history: std::array::from_fn(|_| VecDeque::from(vec![0.0; POINTS + 1])),
            last_tick: Instant::now(),
            interval_ms: RATES_MS[0],
            lag_ms: 0.0,
            changed: Instant::now(),
        }
    }
}

fn memory_percent(meminfo: &str) -> (i32, i32) {
    let kb = |key: &str| -> u64 {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let used_pct = |total: u64, free: u64| if total == 0 { 0 } else { (total.saturating_sub(free) * 100 / total) as i32 };
    (used_pct(kb("MemTotal"), kb("MemAvailable")), used_pct(kb("SwapTotal"), kb("SwapFree")))
}

struct DiskStat {
    name: String,
    io_ms: u64,
    sectors: u64,
}

/// Whole physical disks only: partitions, loop, ram and device-mapper devices would double count.
fn is_whole_disk(name: &str) -> bool {
    let digits_after = |prefix: &str| name.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
    let letters_after = |prefix: &str| name.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_lowercase()));
    letters_after("sd")
        || letters_after("vd")
        || letters_after("hd")
        || digits_after("mmcblk")
        || name.strip_prefix("nvme").is_some_and(|rest| rest.split_once('n').is_some_and(|(c, n)| !c.is_empty() && !n.is_empty() && c.chars().chain(n.chars()).all(|ch| ch.is_ascii_digit())))
}

fn parse_diskstats(text: &str) -> Vec<DiskStat> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let name = f.get(2)?;
            if !is_whole_disk(name) {
                return None;
            }
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
            Some(DiskStat { name: name.to_string(), io_ms: num(12)?, sectors: num(5)? + num(9)? })
        })
        .collect()
}

fn format_rate(bytes_per_s: f64) -> String {
    const KB: f64 = 1024.0;
    if bytes_per_s < KB {
        "0 KB/s".to_string()
    } else if bytes_per_s < KB * KB {
        format!("{:.0} KB/s", bytes_per_s / KB)
    } else if bytes_per_s < KB * KB * KB {
        format!("{:.1} MB/s", bytes_per_s / KB / KB)
    } else {
        format!("{:.1} GB/s", bytes_per_s / KB / KB / KB)
    }
}

/// A smooth curve through the samples as SVG path commands in pixels: `width` spans `POINTS - 1` samples.
fn smooth_path(points: &VecDeque<f32>, width: f32, height: f32) -> String {
    let dx = width / (POINTS as f32 - 1.0);
    let y = |i: isize| {
        let i = i.clamp(0, points.len() as isize - 1) as usize;
        height * (1.0 - points[i].clamp(0.0, 100.0) / 100.0)
    };
    let mut d = format!("M 0 {:.2}", y(0));
    for i in 0..points.len() as isize - 1 {
        let (p0, p1, p2, p3) = (y(i - 1), y(i), y(i + 1), y(i + 2));
        let c1 = (p1 + (p2 - p0) / 6.0).clamp(0.0, height);
        let c2 = (p2 - (p3 - p1) / 6.0).clamp(0.0, height);
        let x = i as f32 * dx;
        d.push_str(&format!(" C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2}", x + dx / 3.0, c1, x + dx * 2.0 / 3.0, c2, x + dx, p2));
    }
    d
}

impl Sampler {
    /// CPU, RAM, swap and disk busy percentages, plus the disk's throughput text.
    fn read(&mut self) -> ([f32; METRICS], String) {
        let now = std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|t| parse_stat(&t).into_iter().next())
            .map(|(_, busy, total)| (busy, total));
        let cpu = match (self.prev, now) {
            (Some(prev), Some(now)) => percent(prev, now),
            _ => 0,
        };
        self.prev = now;
        let (ram, swap) = std::fs::read_to_string("/proc/meminfo").map(|t| memory_percent(&t)).unwrap_or((0, 0));

        let stats = std::fs::read_to_string("/proc/diskstats").map(|t| parse_diskstats(&t)).unwrap_or_default();
        let at = Instant::now();
        let (mut busy, mut bytes_per_s) = (0.0_f32, 0.0_f64);
        if let Some((then, before)) = &self.disks {
            let secs = at.duration_since(*then).as_secs_f64().max(0.001);
            for disk in &stats {
                if let Some(old) = before.iter().find(|d| d.name == disk.name) {
                    busy = busy.max((disk.io_ms.saturating_sub(old.io_ms) as f64 / (secs * 10.0)) as f32);
                    bytes_per_s += disk.sectors.saturating_sub(old.sectors) as f64 * 512.0 / secs;
                }
            }
        }
        self.disks = Some((at, stats));
        ([cpu as f32, ram as f32, swap as f32, busy.clamp(0.0, 100.0)], format_rate(bytes_per_s))
    }

    /// Adapts the tick rate to how late ticks arrive: late ticks mean the UI thread is saturated.
    fn adapt_rate(&mut self) {
        let now = Instant::now();
        let dt_ms = now.duration_since(self.last_tick).as_secs_f32() * 1000.0;
        self.last_tick = now;
        self.lag_ms = self.lag_ms * 0.9 + (dt_ms - self.interval_ms as f32) * 0.1;
        let slot = RATES_MS.iter().position(|r| *r == self.interval_ms).unwrap_or(0);
        if self.lag_ms > 6.0 && slot + 1 < RATES_MS.len() {
            self.interval_ms = RATES_MS[slot + 1];
            self.lag_ms = 0.0;
            self.changed = now;
        } else if self.lag_ms < 1.5 && slot > 0 && now.duration_since(self.changed) > Duration::from_secs(3) {
            self.interval_ms = RATES_MS[slot - 1];
            self.changed = now;
        }
    }

    /// Call at the card's tick rate with the trace area's size in pixels: returns the scroll phase
    /// and, every 200 ms, fresh values and traces.
    pub(crate) fn tick(&mut self, width: f32, height: f32) -> Tick {
        self.adapt_rate();
        let elapsed = self.last.elapsed();
        let fresh = (elapsed >= SAMPLE_EVERY).then(|| {
            self.last = if elapsed > SAMPLE_EVERY * 3 { Instant::now() } else { self.last + SAMPLE_EVERY };
            let (values, disk_rate) = self.read();
            for (history, value) in self.history.iter_mut().zip(values) {
                history.pop_front();
                history.push_back(value);
            }
            Fresh {
                values: values.map(|v| v as i32),
                paths: std::array::from_fn(|i| smooth_path(&self.history[i], width, height)),
                disk_rate,
            }
        });
        let phase = (self.last.elapsed().as_secs_f32() / SAMPLE_EVERY.as_secs_f32()).clamp(0.0, 1.0);
        Tick { interval_ms: self.interval_ms, phase, fresh }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_and_swap_are_percent_used() {
        let text = "MemTotal:       16000000 kB\nMemFree: 1000 kB\nMemAvailable:    4000000 kB\nSwapTotal:       8000000 kB\nSwapFree:        6000000 kB\n";
        assert_eq!(memory_percent(text), (75, 25));
        assert_eq!(memory_percent("MemTotal: 0 kB\n"), (0, 0));
    }

    #[test]
    fn only_whole_disks_are_counted() {
        for yes in ["sda", "vdb", "nvme0n1", "mmcblk0"] {
            assert!(is_whole_disk(yes), "{yes}");
        }
        for no in ["sda1", "nvme0n1p2", "mmcblk0p1", "loop0", "dm-0", "zram0", "sr0"] {
            assert!(!is_whole_disk(no), "{no}");
        }
    }

    #[test]
    fn diskstats_lines_give_busy_time_and_sectors() {
        let text = "   8  0 sda 100 0 2000 50 40 0 800 30 0 120 80\n   8  1 sda1 1 0 8 0 0 0 0 0 0 0 0\n 259  0 nvme0n1 5 0 16 1 6 0 32 2 0 7 3\n";
        let disks = parse_diskstats(text);
        assert_eq!(disks.len(), 2);
        assert_eq!((disks[0].io_ms, disks[0].sectors), (120, 2800));
        assert_eq!((disks[1].name.as_str(), disks[1].sectors), ("nvme0n1", 48));
    }

    #[test]
    fn rates_are_human_readable() {
        assert_eq!(format_rate(0.0), "0 KB/s");
        assert_eq!(format_rate(512.0 * 1024.0), "512 KB/s");
        assert_eq!(format_rate(12.4 * 1024.0 * 1024.0), "12.4 MB/s");
    }

    #[test]
    fn trace_is_one_smooth_curve_through_every_sample() {
        let points: VecDeque<f32> = VecDeque::from(vec![0.0, 50.0, 100.0, 25.0]);
        let path = smooth_path(&points, 590.0, 20.0);
        assert!(path.starts_with("M 0 20.00"), "{path}");
        assert_eq!(path.matches(" C ").count(), 3);
        assert!(path.ends_with(" 15.00"), "{path}");
    }

    #[test]
    fn curve_handles_stay_inside_the_chart() {
        let points: VecDeque<f32> = VecDeque::from(vec![0.0, 100.0, 0.0, 100.0]);
        let path = smooth_path(&points, 100.0, 40.0);
        let ys: Vec<f32> = path.split(" C ").skip(1).flat_map(|seg| seg.split_whitespace().skip(1).step_by(2).filter_map(|t| t.parse().ok()).collect::<Vec<f32>>()).collect();
        assert!(ys.iter().all(|y| (0.0..=40.0).contains(y)), "{ys:?}");
    }
}
