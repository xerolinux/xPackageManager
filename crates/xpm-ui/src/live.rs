//! Live sched_ext readings for the Kernel Schedulers page: kernel state files, CPU load, load average.

use std::collections::VecDeque;
use std::sync::Mutex;

const SYSFS: &str = "/sys/kernel/sched_ext";
const HISTORY_LEN: usize = 60;

pub(crate) struct Live {
    pub state: String,
    pub ops: String,
    pub switch_all: bool,
    pub enable_seq: String,
    pub hotplug_seq: String,
    pub rejected: String,
    pub load: String,
    pub total: i32,
    pub cpus: Vec<(String, i32)>,
    pub history: Vec<i32>,
}

struct Sampler {
    prev: Vec<(u64, u64)>,
    history: VecDeque<i32>,
}

static SAMPLER: Mutex<Sampler> = Mutex::new(Sampler { prev: Vec::new(), history: VecDeque::new() });

fn read(name: &str) -> Option<String> {
    std::fs::read_to_string(format!("{SYSFS}/{name}")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// `(label, busy, total)` jiffies per `cpuN` line, with the aggregate `cpu` line first.
fn parse_stat(text: &str) -> Vec<(String, u64, u64)> {
    text.lines()
        .filter(|l| l.starts_with("cpu"))
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let label = parts.next()?.to_string();
            let v: Vec<u64> = parts.take(8).filter_map(|p| p.parse().ok()).collect();
            if v.len() < 5 {
                return None;
            }
            let total: u64 = v.iter().sum();
            let idle = v[3] + v[4];
            Some((label, total - idle, total))
        })
        .collect()
}

fn percent(prev: (u64, u64), now: (u64, u64)) -> i32 {
    let total = now.1.saturating_sub(prev.1);
    if total == 0 {
        return 0;
    }
    (now.0.saturating_sub(prev.0) * 100 / total).min(100) as i32
}

/// Operator-facing scheduler name from the kernel's ops string (`tickless_1.1.4_x86_64_unknown_linux_gnu`).
fn ops_name(ops: &str) -> String {
    ops.split('_').next().unwrap_or(ops).to_string()
}

pub(crate) fn sample() -> Live {
    let stat = std::fs::read_to_string("/proc/stat").map(|t| parse_stat(&t)).unwrap_or_default();
    let mut sampler = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    let now: Vec<(u64, u64)> = stat.iter().map(|(_, busy, total)| (*busy, *total)).collect();
    let pcts: Vec<i32> =
        now.iter().enumerate().map(|(i, n)| sampler.prev.get(i).map_or(0, |p| percent(*p, *n))).collect();
    sampler.prev = now;
    let total = pcts.first().copied().unwrap_or(0);
    if !stat.is_empty() {
        sampler.history.push_back(total);
        while sampler.history.len() > HISTORY_LEN {
            sampler.history.pop_front();
        }
    }
    let cpus = stat.iter().zip(&pcts).skip(1).map(|((label, _, _), pct)| (label.clone(), *pct)).collect();
    let load = std::fs::read_to_string("/proc/loadavg")
        .map(|t| t.split_whitespace().take(3).collect::<Vec<_>>().join("  "))
        .unwrap_or_default();
    Live {
        state: read("state").unwrap_or_else(|| "unavailable".into()),
        ops: read("root/ops").map(|o| ops_name(&o)).unwrap_or_default(),
        switch_all: read("switch_all").is_some_and(|s| s == "1"),
        enable_seq: read("enable_seq").unwrap_or_else(|| "-".into()),
        hotplug_seq: read("hotplug_seq").unwrap_or_else(|| "-".into()),
        rejected: read("nr_rejected").unwrap_or_else(|| "-".into()),
        load,
        total,
        cpus,
        history: sampler.history.iter().copied().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_lines_become_busy_and_total() {
        let text = "cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 50 0 50 350 50 0 0 0 0 0\nintr 5\n";
        let rows = parse_stat(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("cpu".to_string(), 200, 1000));
        assert_eq!(rows[1].0, "cpu0");
    }

    #[test]
    fn percent_uses_the_delta_between_samples() {
        assert_eq!(percent((200, 1000), (300, 1200)), 50);
        assert_eq!(percent((200, 1000), (200, 1000)), 0);
        assert_eq!(percent((0, 0), (500, 100)), 100);
    }

    #[test]
    fn ops_string_is_trimmed_to_the_scheduler() {
        assert_eq!(ops_name("tickless_1.1.4_x86_64_unknown_linux_gnu"), "tickless");
        assert_eq!(ops_name("simple"), "simple");
    }
}
