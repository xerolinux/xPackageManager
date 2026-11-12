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
    SchedulerInfo { id: "scx_tickless", name: "Tickless", category: "Servers / HPC", description: "A server scheduler built to cut OS noise and keep workloads isolated. All scheduling events go through a small pool of primary CPUs (CPU 0 by default, set with --primary-domain), so the scheduler tick can be switched off on every other CPU. Preemption is only signalled when several tasks compete for one tickless CPU, and --frequency sets how often that check runs. The kernel must be booted with nohz_full for ticks to really stop, and nohz_full adds syscall overhead. Meant for cloud, virtualization and HPC, not for latency-sensitive desktop use. Still experimental." },
    SchedulerInfo { id: "scx_beerland", name: "Beerland", category: "Cache-Heavy / Many Cores", description: "Short for BPF-Enhanced Execution Runtime Locality-Aware Non-blocking Dispatcher. It keeps a deadline-ordered queue per CPU and lets a task migrate only when it wakes up and the system is not saturated. Once the system saturates, idle CPUs start pulling the earliest-deadline task from remote queues. Best for cache-intensive workloads and machines with a large number of CPUs. Upstream marks it production ready." },
    SchedulerInfo { id: "scx_cake", name: "Cake", category: "Gaming", description: "A gaming-first scheduler that also works for everyday use, named after the CAKE network queue manager. It aims for three things: no task waits while a CPU sits free, tasks stay near their warm cache, and CPU time stays fair. It decides from task behaviour and CPU topology, never from a game name or profile, and needs no flags because the default is the policy." },
    SchedulerInfo { id: "scx_flow", name: "Flow", category: "Desktop / Latency-Sensitive", description: "Runs the task with the earliest strict key, the earlier of its deadline and its virtual time. New arrivals pass a RED (random early detection) admission gate with five priority tiers, so short bursts of work reach a CPU sooner. Aimed at latency-sensitive applications, general desktop use and mixed batch workloads." },
    SchedulerInfo { id: "scx_forge", name: "Forge", category: "Experimental / AI-Tuned", description: "A base scheduler meant to be rewritten in place by an AI optimizer (scx_forge_agent), which builds, runs and scores each change. The default policy uses per-CPU queues with work stealing and a fixed slice. It runs, but upstream does not intend it as a production scheduler unchanged." },
    SchedulerInfo { id: "scx_mlfq", name: "MLFQ", category: "Gaming / Desktop", description: "A knob-free multilevel feedback queue built on EEVDF-style virtual time. Tasks are sorted into three per-CPU queues: interactive (1 ms slice), unclassified (2 ms) and CPU-bound (4 ms). A regression tree trained on your own machine predicts each task's next CPU burst, and tasks that recently submitted GPU work count as interactive. Wakeup boosts, demotion and aging keep the desktop responsive while compiles and updates run in the background." },
    SchedulerInfo { id: "scx_pandemonium", name: "Pandemonium", category: "Desktop / Adaptive", description: "An adaptive scheduler that prices every decision in nanoseconds against a live service bound and re-tunes that bound in real time. It has no task classifier: slice, placement and preemption come from measured service, queue backlog and wait time. Stall detection is CoDel-inspired, and topology-aware placement helps pipe and IPC heavy workloads. Pass --no-adaptive to run only the BPF part." },
    SchedulerInfo { id: "scx_p2dq", name: "P2DQ", category: "General Purpose", description: "A general-purpose pick-two load balancer with multi-layer queueing. Each last-level cache has CPU, LLC and migration queues ordered by virtual runtime. When a CPU runs dry it randomly picks two LLCs or NUMA nodes and pulls work from the busier one, which spreads load cheaply across large systems." },
    SchedulerInfo { id: "scx_chaos", name: "Chaos", category: "Testing / Debugging", description: "A scheduler that deliberately adds latency to expose race conditions. Traits include random delays, random CPU frequency scaling, performance degradation and kprobe-based delays. Without arguments it adds nothing and gives a baseline. Experimental alpha: it is built to degrade performance and can destabilise the system, so never use it for daily work." },
    SchedulerInfo { id: "scx_eevdf", name: "EEVDF", category: "Reference / Comparison", description: "A port of the kernel's own EEVDF scheduler to sched_ext. Tasks are ordered by virtual deadline and keep the lag they carried out of their previous queue, following the kernel's fair.c rules. Several rules can be switched off from the command line, so it is mainly useful for comparing sched_ext schedulers against the kernel default." },
    SchedulerInfo { id: "scx_mavd", name: "MAVD", category: "Gaming / Low Latency", description: "A variant of LAVD, the latency-criticality aware virtual deadline scheduler. Rust handles options and statistics while the BPF side makes every scheduling decision, giving latency-critical tasks earlier virtual deadlines." },
    SchedulerInfo { id: "scx_nitosis", name: "Nitosis", category: "Servers / Containers", description: "A dynamic affinity scheduler. Cgroups are assigned to a dynamic number of cells, and each cell gets a dynamic set of CPUs. The BPF side does simple virtual-time scheduling inside each cell, while userspace decides when cells should be merged or split and which CPUs they receive." },
];
