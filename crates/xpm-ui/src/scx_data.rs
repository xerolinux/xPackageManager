//! Static reference data for the Kernel Schedulers page, taken from the XeroLinux scx-tool.

pub(crate) struct FlagInfo {
    pub scheduler: &'static str,
    pub flag: &'static str,
    pub required: bool,
    pub description: &'static str,
}

pub(crate) struct SchedulerInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub category: &'static str,
    pub description: &'static str,
}

pub(crate) const FLAGS: &[FlagInfo] = &[
    FlagInfo { scheduler: "scx_bpfland", flag: "--slice-us", required: false, description: "Tune slice duration in microseconds. Lower = more responsive. Default: 2000. Try 500-1000 for low-latency." },
    FlagInfo { scheduler: "scx_bpfland", flag: "--freq-to-lowfreq", required: false, description: "CPU frequency governor hint: 'performance' keeps clocks high, 'powersave' allows down-clocking." },
    FlagInfo { scheduler: "scx_bpfland", flag: "--partial", required: false, description: "Enable partial scheduling mode. Only selected tasks go through the BPF scheduler." },
    FlagInfo { scheduler: "scx_bpfland", flag: "--nr-cpus-big", required: false, description: "Number of 'big' CPUs for asymmetric systems (e.g. big.LITTLE). Set to 0 to disable." },
    FlagInfo { scheduler: "scx_lavd", flag: "--no-preempt-idle", required: false, description: "Prevents idle CPUs from running tasks from other scheduling domains. Useful for kernel task isolation." },
    FlagInfo { scheduler: "scx_lavd", flag: "--autopilot", required: false, description: "Automatically tune scheduler parameters based on detected workload characteristics." },
    FlagInfo { scheduler: "scx_lavd", flag: "--slice-us", required: false, description: "Slice duration in microseconds. Controls task scheduling granularity. Lower = more responsive." },
    FlagInfo { scheduler: "scx_lavd", flag: "--performance", required: false, description: "Prefer performance over power efficiency. Keeps CPUs running at higher frequencies." },
    FlagInfo { scheduler: "scx_nest", flag: "--print-cpumask", required: false, description: "Print per-core idle CPUmask to stdout. Useful for debugging CPU affinity issues." },
    FlagInfo { scheduler: "scx_nest", flag: "--delay-us", required: false, description: "How long a CPU waits idle before it is considered for task removal. Lower = lower power, fewer task dispatches." },
    FlagInfo { scheduler: "scx_nest", flag: "--lowlatency", required: false, description: "Target percentage of idle CPUs to maintain. Higher = more power usage, lower = fewer task dispatch opportunities." },
    FlagInfo { scheduler: "scx_rusty", flag: "--no-preempt", required: false, description: "Disable slice preemption. Tasks spread freely across all cores rather than being consolidated." },
    FlagInfo { scheduler: "scx_rusty", flag: "--load-half-life", required: false, description: "Load tracking half-life in seconds. Lower values track workload changes faster." },
    FlagInfo { scheduler: "scx_rusty", flag: "--verbose", required: false, description: "Enable verbose logging output. Useful for debugging scheduler behaviour." },
    FlagInfo { scheduler: "scx_tickless", flag: "--slice-us", required: false, description: "Tune slice duration in microseconds for the tickless scheduler. Lower = more responsive." },
    FlagInfo { scheduler: "scx_tickless", flag: "--no-preempt", required: false, description: "Disable slice preemption. Tasks spread freely across all cores rather than being consolidated." },
    FlagInfo { scheduler: "scx_layered", flag: "--stats", required: false, description: "Print per-layer scheduling statistics periodically. Useful for performance analysis and tuning." },
    FlagInfo { scheduler: "scx_layered", flag: "--slice-us", required: false, description: "Slice duration in microseconds. Controls how long a task runs before being preempted." },
    FlagInfo { scheduler: "scx_layered", flag: "--verbose", required: false, description: "Enable verbose logging. Helps diagnose layer assignment and scheduling decisions." },
    FlagInfo { scheduler: "scx_userland", flag: "--interval-us", required: false, description: "Scheduler decision interval in microseconds. Default: 1000. Reduce for interactive use, increase for throughput." },
    FlagInfo { scheduler: "scx_userland", flag: "--partial", required: false, description: "Enable partial scheduling. Only selected tasks are managed by the userland scheduler." },
    FlagInfo { scheduler: "scx_flatcg", flag: "--nr-dom-slabs", required: false, description: "Number of per-domain scheduling slabs. Affects memory usage and scheduling granularity." },
    FlagInfo { scheduler: "scx_flatcg", flag: "--slice-us", required: false, description: "Slice duration in microseconds. Controls task scheduling time quantum." },
    FlagInfo { scheduler: "scx_simple", flag: "--fifo", required: false, description: "Run all tasks in FIFO order. Useful for real-time workloads or debugging scheduler behaviour." },
    FlagInfo { scheduler: "scx_simple", flag: "--slice-us", required: false, description: "Slice duration in microseconds for simple round-robin scheduling." },
    FlagInfo { scheduler: "scx_chaos", flag: "--seed", required: false, description: "Random seed for chaos scheduling decisions. Use a fixed value for reproducible testing." },
    FlagInfo { scheduler: "scx_chaos", flag: "--slice-us", required: false, description: "Slice duration in microseconds. Lower values increase scheduling frequency and chaos." },
];

pub(crate) const INFO: &[SchedulerInfo] = &[
    SchedulerInfo { id: "scx_bpfland", name: "BPFland", category: "Gaming / Interactive", description: "A vruntime-based scheduler prioritizing interactive workloads by identifying tasks that block frequently. It considers cache layout when assigning cores, excelling under heavy load and gaming scenarios while supporting hybrid CPU topologies." },
    SchedulerInfo { id: "scx_lavd", name: "LAVD", category: "Gaming / Low Latency", description: "Designed for gaming and latency-sensitive tasks. Calculates a 'latency criticality' score per task and assigns virtual deadlines accordingly. Features Core Compaction for power savings and Autopilot mode for automatic power profile adjustment." },
    SchedulerInfo { id: "scx_rusty", name: "Rusty", category: "Desktop / Server", description: "A load-balancing focused scheduler that groups CPUs by Last-Level Cache and maintains task locality. Highly tunable with flags, it includes Autopower mode and suits general desktop and server workloads." },
    SchedulerInfo { id: "scx_rustland", name: "Rustland", category: "Userspace / Educational", description: "The userspace predecessor to BPFland with similar scheduling logic but running in userspace. More readable for understanding scheduler behaviour, though with slight throughput trade-offs compared to its BPF counterpart." },
    SchedulerInfo { id: "scx_flash", name: "Flash", category: "Desktop / Soft Real-Time", description: "Emphasises fairness and performance predictability over prioritising interactive tasks. It replaced the lowlatency mode in BPFland and targets soft real-time workloads such as audio processing." },
    SchedulerInfo { id: "scx_layered", name: "Layered", category: "Power Users", description: "A highly configurable scheduler enabling task classification into named 'layers' with independent scheduling policies per layer. Suited for power users needing fine-grained control over how different workloads are handled." },
    SchedulerInfo { id: "scx_cosmos", name: "Cosmos", category: "General Purpose", description: "A lightweight scheduler optimising task-to-CPU locality, maintaining cache warmth and reducing migration overhead. Designed for general-purpose desktop use." },
    SchedulerInfo { id: "scx_simple", name: "Simple", category: "Reference / Learning", description: "A minimal reference scheduler serving as documentation and learning material. Maintains per-CPU locality but is not intended for production use - useful for understanding how sched-ext schedulers are built." },
];
