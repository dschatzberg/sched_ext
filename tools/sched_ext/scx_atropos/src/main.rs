// Copyright (c) Meta Platforms, Inc. and affiliates.

// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.
#[path = "bpf/.output/atropos.skel.rs"]
mod atropos;
pub use atropos::*;
pub mod atropos_sys;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::CStr;
use std::ops::Bound::Included;
use std::ops::Bound::Unbounded;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use bitvec::prelude::*;
use clap::Parser;
use libbpf_rs::skel::OpenSkel as _;
use libbpf_rs::skel::Skel as _;
use libbpf_rs::skel::SkelBuilder as _;
use log::info;
use log::trace;
use log::warn;
use ordered_float::OrderedFloat;

/// Atropos is a multi-domain BPF / userspace hybrid scheduler where the BPF
/// part does simple round robin in each domain and the userspace part
/// calculates the load factor of each domain and tells the BPF part how to load
/// balance the domains.
///
/// This scheduler demonstrates dividing scheduling logic between BPF and
/// userspace and using rust to build the userspace part. An earlier variant of
/// this scheduler was used to balance across six domains, each representing a
/// chiplet in a six-chiplet AMD processor, and could match the performance of
/// production setup using CFS.
///
/// WARNING: Atropos currently assumes that all domains have equal
/// processing power and at similar distances from each other. This
/// limitation will be removed in the future.
#[derive(Debug, Parser)]
struct Opts {
    /// Scheduling slice duration in microseconds.
    #[clap(short = 's', long, default_value = "20000")]
    slice_us: u64,

    /// Monitoring and load balance interval in seconds.
    #[clap(short = 'i', long, default_value = "2.0")]
    interval: f64,

    /// Tuner runs at higher frequency than the load balancer to dynamically
    /// tune scheduling behavior. Tuning interval in seconds.
    #[clap(short = 'I', long, default_value = "0.1")]
    tune_interval: f64,

    /// Build domains according to how CPUs are grouped at this cache level
    /// as determined by /sys/devices/system/cpu/cpuX/cache/indexI/id.
    #[clap(short = 'c', long, default_value = "3")]
    cache_level: u32,

    /// Instead of using cache locality, set the cpumask for each domain
    /// manually, provide multiple --cpumasks, one for each domain. E.g.
    /// --cpumasks 0xff_00ff --cpumasks 0xff00 will create two domains with
    /// the corresponding CPUs belonging to each domain. Each CPU must
    /// belong to precisely one domain.
    #[clap(short = 'C', long, num_args = 1.., conflicts_with = "cache_level")]
    cpumasks: Vec<String>,

    /// When non-zero, enable greedy task stealing. When a domain is idle, a
    /// cpu will attempt to steal tasks from a domain with at least
    /// greedy_threshold tasks enqueued. These tasks aren't permanently
    /// stolen from the domain.
    #[clap(short = 'g', long, default_value = "1")]
    greedy_threshold: u32,

    /// The load decay factor. Every interval, the existing load is decayed
    /// by this factor and new load is added. Must be in the range [0.0,
    /// 0.99]. The smaller the value, the more sensitive load calculation
    /// is to recent changes. When 0.0, history is ignored and the load
    /// value from the latest period is used directly.
    #[clap(long, default_value = "0.5")]
    load_decay_factor: f64,

    /// Disable load balancing. Unless disabled, periodically userspace will
    /// calculate the load factor of each domain and instruct BPF which
    /// processes to move.
    #[clap(long, action = clap::ArgAction::SetTrue)]
    no_load_balance: bool,

    /// Put per-cpu kthreads directly into local dsq's.
    #[clap(short = 'k', long, action = clap::ArgAction::SetTrue)]
    kthreads_local: bool,

    /// In recent kernels (>=v6.6), the kernel is responsible for balancing
    /// kworkers across L3 cache domains. Exclude them from load-balancing
    /// to avoid conflicting operations. Greedy executions still apply.
    #[clap(short = 'b', long, action = clap::ArgAction::SetTrue)]
    balanced_kworkers: bool,

    /// Use FIFO scheduling instead of weighted vtime scheduling.
    #[clap(short = 'f', long, action = clap::ArgAction::SetTrue)]
    fifo_sched: bool,

    /// Idle CPUs with utilization lower than this will get remote tasks
    /// directly pushed on them. 0 disables, 100 enables always.
    #[clap(short = 'D', long, default_value = "90.0")]
    direct_greedy_under: f64,

    /// Idle CPUs with utilization lower than this may get kicked to
    /// accelerate stealing when a task is queued on a saturated remote
    /// domain. 0 disables, 100 enables always.
    #[clap(short = 'K', long, default_value = "100.0")]
    kick_greedy_under: f64,

    /// If specified, only tasks which have their scheduling policy set to
    /// SCHED_EXT using sched_setscheduler(2) are switched. Otherwise, all
    /// tasks are switched.
    #[clap(short = 'p', long, action = clap::ArgAction::SetTrue)]
    partial: bool,

    /// Enable verbose output including libbpf details. Specify multiple
    /// times to increase verbosity.
    #[clap(short = 'v', long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn now_monotonic() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    assert!(ret == 0);
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

fn clear_map(map: &mut libbpf_rs::Map) {
    // XXX: libbpf_rs has some design flaw that make it impossible to
    // delete while iterating despite it being safe so we alias it here
    let deleter: &mut libbpf_rs::Map = unsafe { &mut *(map as *mut _) };
    for key in map.keys() {
        let _ = deleter.delete(&key);
    }
}

fn format_cpumask(cpumask: &[u64], nr_cpus: usize) -> String {
    cpumask
        .iter()
        .take((nr_cpus + 64) / 64)
        .rev()
        .fold(String::new(), |acc, x| format!("{} {:016X}", acc, x))
}

// Neither procfs or fb_procfs can determine per-CPU utilization reliably
// with CPU hot[un]plugs. Roll our own.
//
// https://github.com/eminence/procfs/issues/274
// https://github.com/facebookincubator/below/issues/8190
#[derive(Clone, Debug, Default)]
struct MyCpuStat {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
}

impl MyCpuStat {
    fn busy_and_total(&self) -> (u64, u64) {
        let busy = self.user + self.system + self.nice + self.irq + self.softirq + self.steal;
        (busy, self.idle + busy + self.iowait)
    }

    fn calc_util(&self, prev: &MyCpuStat) -> f64 {
        let (curr_busy, curr_total) = self.busy_and_total();
        let (prev_busy, prev_total) = prev.busy_and_total();
        let busy = curr_busy - prev_busy;
        let total = curr_total - prev_total;
        if total > 0 {
            ((busy as f64) / (total as f64)).clamp(0.0, 1.0)
        } else {
            1.0
        }
    }
}

#[derive(Clone, Debug, Default)]
struct MyProcStat {
    total: MyCpuStat,
    cpus: BTreeMap<usize, MyCpuStat>,
}

impl MyProcStat {
    fn read() -> Result<Self> {
        let mut result: MyProcStat = Default::default();
        for line in std::fs::read_to_string("/proc/stat")?.lines() {
            let mut toks = line.split_whitespace();

            let key = toks.next().ok_or(anyhow!("no key"))?;
            if !key.starts_with("cpu") {
                break;
            }

            let cputime = MyCpuStat {
                user: toks.next().ok_or(anyhow!("missing"))?.parse::<u64>()?,
                nice: toks.next().ok_or(anyhow!("missing"))?.parse::<u64>()?,
                system: toks.next().ok_or(anyhow!("missing"))?.parse::<u64>()?,
                idle: toks.next().ok_or(anyhow!("missing"))?.parse::<u64>()?,
                iowait: toks.next().ok_or(anyhow!("missing"))?.parse::<u64>()?,
                irq: toks.next().ok_or(anyhow!("missing"))?.parse::<u64>()?,
                softirq: toks.next().ok_or(anyhow!("missing"))?.parse::<u64>()?,
                steal: toks.next().ok_or(anyhow!("missing"))?.parse::<u64>()?,
            };

            if key.len() == 3 {
                result.total = cputime;
            } else {
                result.cpus.insert(key[3..].parse::<usize>()?, cputime);
            }
        }
        Ok(result)
    }
}

#[derive(Debug)]
struct Topology {
    nr_cpus: usize,
    nr_doms: usize,
    dom_cpus: Vec<BitVec<u64, Lsb0>>,
    cpu_dom: Vec<Option<usize>>,
}

impl Topology {
    fn from_cpumasks(cpumasks: &[String], nr_cpus: usize) -> Result<Self> {
        if cpumasks.len() > atropos_sys::MAX_DOMS as usize {
            bail!(
                "Number of requested domains ({}) is greater than MAX_DOMS ({})",
                cpumasks.len(),
                atropos_sys::MAX_DOMS
            );
        }
        let mut cpu_dom = vec![None; nr_cpus];
        let mut dom_cpus =
            vec![bitvec![u64, Lsb0; 0; atropos_sys::MAX_CPUS as usize]; cpumasks.len()];
        for (dom, cpumask) in cpumasks.iter().enumerate() {
            let hex_str = {
                let mut tmp_str = cpumask
                    .strip_prefix("0x")
                    .unwrap_or(cpumask)
                    .replace('_', "");
                if tmp_str.len() % 2 != 0 {
                    tmp_str = "0".to_string() + &tmp_str;
                }
                tmp_str
            };
            let byte_vec = hex::decode(&hex_str)
                .with_context(|| format!("Failed to parse cpumask: {}", cpumask))?;

            for (index, &val) in byte_vec.iter().rev().enumerate() {
                let mut v = val;
                while v != 0 {
                    let lsb = v.trailing_zeros() as usize;
                    v &= !(1 << lsb);
                    let cpu = index * 8 + lsb;
                    if cpu > nr_cpus {
                        bail!(
                            concat!(
                                "Found cpu ({}) in cpumask ({}) which is larger",
                                " than the number of cpus on the machine ({})"
                            ),
                            cpu,
                            cpumask,
                            nr_cpus
                        );
                    }
                    if let Some(other_dom) = cpu_dom[cpu] {
                        bail!(
                            "Found cpu ({}) with domain ({}) but also in cpumask ({})",
                            cpu,
                            other_dom,
                            cpumask
                        );
                    }
                    cpu_dom[cpu] = Some(dom);
                    dom_cpus[dom].set(cpu, true);
                }
            }
            dom_cpus[dom].set_uninitialized(false);
        }

        for (cpu, dom) in cpu_dom.iter().enumerate() {
            if dom.is_none() {
                bail!(
                    "CPU {} not assigned to any domain. Make sure it is covered by some --cpumasks argument.",
                    cpu
                );
            }
        }

        Ok(Self {
            nr_cpus,
            nr_doms: dom_cpus.len(),
            dom_cpus,
            cpu_dom,
        })
    }

    fn from_cache_level(level: u32, nr_cpus: usize) -> Result<Self> {
        let mut cpu_to_cache = vec![]; // (cpu_id, Option<cache_id>)
        let mut cache_ids = BTreeSet::<usize>::new();
        let mut nr_offline = 0;

        // Build cpu -> cache ID mapping.
        for cpu in 0..nr_cpus {
            let path = format!("/sys/devices/system/cpu/cpu{}/cache/index{}/id", cpu, level);
            let id = match std::fs::read_to_string(&path) {
                Ok(val) => Some(val.trim().parse::<usize>().with_context(|| {
                    format!("Failed to parse {:?}'s content {:?}", &path, &val)
                })?),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    nr_offline += 1;
                    None
                }
                Err(e) => return Err(e).with_context(|| format!("Failed to open {:?}", &path)),
            };

            cpu_to_cache.push(id);
            if id.is_some() {
                cache_ids.insert(id.unwrap());
            }
        }

        info!(
            "CPUs: online/possible = {}/{}",
            nr_cpus - nr_offline,
            nr_cpus
        );

        // Cache IDs may have holes. Assign consecutive domain IDs to
        // existing cache IDs.
        let mut cache_to_dom = BTreeMap::<usize, usize>::new();
        let mut nr_doms = 0;
        for cache_id in cache_ids.iter() {
            cache_to_dom.insert(*cache_id, nr_doms);
            nr_doms += 1;
        }

        if nr_doms > atropos_sys::MAX_DOMS as usize {
            bail!(
                "Total number of doms {} is greater than MAX_DOMS ({})",
                nr_doms,
                atropos_sys::MAX_DOMS
            );
        }

        // Build and return dom -> cpumask and cpu -> dom mappings.
        let mut dom_cpus =
            vec![bitvec![u64, Lsb0; 0; atropos_sys::MAX_CPUS as usize]; nr_doms as usize];
        let mut cpu_dom = vec![];

        for cpu in 0..nr_cpus {
            match cpu_to_cache[cpu] {
                Some(cache_id) => {
                    let dom_id = cache_to_dom[&cache_id];
                    dom_cpus[dom_id].set(cpu, true);
                    cpu_dom.push(Some(dom_id));
                }
                None => {
                    dom_cpus[0].set(cpu, true);
                    cpu_dom.push(None);
                }
            }
        }

        Ok(Self {
            nr_cpus,
            nr_doms: dom_cpus.len(),
            dom_cpus,
            cpu_dom,
        })
    }
}

struct Tuner {
    top: Arc<Topology>,
    direct_greedy_under: f64,
    kick_greedy_under: f64,
    prev_cpu_stats: BTreeMap<usize, MyCpuStat>,
    dom_utils: Vec<f64>,
}

impl Tuner {
    fn new(top: Arc<Topology>, opts: &Opts) -> Result<Self> {
        Ok(Self {
            direct_greedy_under: opts.direct_greedy_under / 100.0,
            kick_greedy_under: opts.kick_greedy_under / 100.0,
            prev_cpu_stats: MyProcStat::read()?.cpus,
            dom_utils: vec![0.0; top.nr_doms],
            top,
        })
    }

    fn step(&mut self, skel: &mut AtroposSkel) -> Result<()> {
        let curr_cpu_stats = MyProcStat::read()?.cpus;
        let ti = &mut skel.bss().tune_input;
        let mut dom_nr_cpus = vec![0; self.top.nr_doms];
        let mut dom_util_sum = vec![0.0; self.top.nr_doms];

        for cpu in 0..self.top.nr_cpus {
            // None domain indicates the CPU was offline during
            // initialization and None MyCpuStat indicates the CPU has gone
            // down since then. Ignore both.
            if let (Some(dom), Some(curr), Some(prev)) = (
                self.top.cpu_dom[cpu],
                curr_cpu_stats.get(&cpu),
                self.prev_cpu_stats.get(&cpu),
            ) {
                dom_nr_cpus[dom] += 1;
                dom_util_sum[dom] += curr.calc_util(prev);
            }
        }

        for dom in 0..self.top.nr_doms {
            // Calculate the domain avg util. If there are no active CPUs,
            // it doesn't really matter. Go with 0.0 as that's less likely
            // to confuse users.
            let util = match dom_nr_cpus[dom] {
                0 => 0.0,
                nr => dom_util_sum[dom] / nr as f64,
            };

            self.dom_utils[dom] = util;

            // This could be implemented better.
            let update_dom_bits = |target: &mut [u64; 8], val: bool| {
                for cpu in 0..self.top.nr_cpus {
                    if let Some(cdom) = self.top.cpu_dom[cpu] {
                        if cdom == dom {
                            if val {
                                target[cpu / 64] |= 1u64 << (cpu % 64);
                            } else {
                                target[cpu / 64] &= !(1u64 << (cpu % 64));
                            }
                        }
                    }
                }
            };

            update_dom_bits(
                &mut ti.direct_greedy_cpumask,
                self.direct_greedy_under > 0.99999 || util < self.direct_greedy_under,
            );
            update_dom_bits(
                &mut ti.kick_greedy_cpumask,
                self.kick_greedy_under > 0.99999 || util < self.kick_greedy_under,
            );
        }

        ti.gen += 1;
        self.prev_cpu_stats = curr_cpu_stats;
        Ok(())
    }
}

#[derive(Debug)]
struct TaskLoad {
    runnable_for: u64,
    load: f64,
}

#[derive(Debug)]
struct TaskInfo {
    pid: i32,
    dom_mask: u64,
    migrated: Cell<bool>,
    is_kworker: bool,
}

struct LoadBalancer<'a, 'b, 'c> {
    maps: AtroposMapsMut<'a>,
    top: Arc<Topology>,
    task_loads: &'b mut BTreeMap<i32, TaskLoad>,
    load_decay_factor: f64,
    skip_kworkers: bool,

    tasks_by_load: Vec<BTreeMap<OrderedFloat<f64>, TaskInfo>>,
    load_avg: f64,
    dom_loads: Vec<f64>,

    imbal: Vec<f64>,
    doms_to_push: BTreeMap<OrderedFloat<f64>, u32>,
    doms_to_pull: BTreeMap<OrderedFloat<f64>, u32>,

    nr_lb_data_errors: &'c mut u64,
}

impl<'a, 'b, 'c> LoadBalancer<'a, 'b, 'c> {
    // If imbalance gets higher than this ratio, try to balance the loads.
    const LOAD_IMBAL_HIGH_RATIO: f64 = 0.10;

    // Aim to transfer this fraction of the imbalance on each round. We want
    // to be gradual to avoid unnecessary oscillations. While this can delay
    // convergence, greedy execution should be able to bridge the temporary
    // gap.
    const LOAD_IMBAL_XFER_TARGET_RATIO: f64 = 0.50;

    // Don't push out more than this ratio of load on each round. While this
    // overlaps with XFER_TARGET_RATIO, XFER_TARGET_RATIO only defines the
    // target and doesn't limit the total load. As long as the transfer
    // reduces load imbalance between the two involved domains, it'd happily
    // transfer whatever amount that can be transferred. This limit is used
    // as the safety cap to avoid draining a given domain too much in a
    // single round.
    const LOAD_IMBAL_PUSH_MAX_RATIO: f64 = 0.50;

    fn new(
        maps: AtroposMapsMut<'a>,
        top: Arc<Topology>,
        task_loads: &'b mut BTreeMap<i32, TaskLoad>,
        load_decay_factor: f64,
        skip_kworkers: bool,
        nr_lb_data_errors: &'c mut u64,
    ) -> Self {
        Self {
            maps,
            task_loads,
            load_decay_factor,
            skip_kworkers,

            tasks_by_load: (0..top.nr_doms).map(|_| BTreeMap::<_, _>::new()).collect(),
            load_avg: 0f64,
            dom_loads: vec![0.0; top.nr_doms],

            imbal: vec![0.0; top.nr_doms],
            doms_to_pull: BTreeMap::new(),
            doms_to_push: BTreeMap::new(),

            nr_lb_data_errors,

            top,
        }
    }

    fn read_task_loads(&mut self, period: Duration) -> Result<()> {
        let now_mono = now_monotonic();
        let task_data = self.maps.task_data();
        let mut this_task_loads = BTreeMap::<i32, TaskLoad>::new();
        let mut load_sum = 0.0f64;
        self.dom_loads = vec![0f64; self.top.nr_doms];

        for key in task_data.keys() {
            if let Some(task_ctx_vec) = task_data
                .lookup(&key, libbpf_rs::MapFlags::ANY)
                .context("Failed to lookup task_data")?
            {
                let task_ctx =
                    unsafe { &*(task_ctx_vec.as_slice().as_ptr() as *const atropos_sys::task_ctx) };
                let pid = i32::from_ne_bytes(
                    key.as_slice()
                        .try_into()
                        .context("Invalid key length in task_data map")?,
                );

                let (this_at, this_for, weight) = unsafe {
                    (
                        std::ptr::read_volatile(&task_ctx.runnable_at as *const u64),
                        std::ptr::read_volatile(&task_ctx.runnable_for as *const u64),
                        std::ptr::read_volatile(&task_ctx.weight as *const u32),
                    )
                };

                let (mut delta, prev_load) = match self.task_loads.get(&pid) {
                    Some(prev) => (this_for - prev.runnable_for, Some(prev.load)),
                    None => (this_for, None),
                };

                // Non-zero this_at indicates that the task is currently
                // runnable. Note that we read runnable_at and runnable_for
                // without any synchronization and there is a small window
                // where we end up misaccounting. While this can cause
                // temporary error, it's unlikely to cause any noticeable
                // misbehavior especially given the load value clamping.
                if this_at > 0 && this_at < now_mono {
                    delta += now_mono - this_at;
                }

                delta = delta.min(period.as_nanos() as u64);
                let this_load = (weight as f64 * delta as f64 / period.as_nanos() as f64)
                    .clamp(0.0, weight as f64);

                let this_load = match prev_load {
                    Some(prev_load) => {
                        prev_load * self.load_decay_factor
                            + this_load * (1.0 - self.load_decay_factor)
                    }
                    None => this_load,
                };

                this_task_loads.insert(
                    pid,
                    TaskLoad {
                        runnable_for: this_for,
                        load: this_load,
                    },
                );

                load_sum += this_load;
                self.dom_loads[task_ctx.dom_id as usize] += this_load;
                // Only record pids that are eligible for load balancing
                if task_ctx.dom_mask == (1u64 << task_ctx.dom_id) {
                    continue;
                }
                self.tasks_by_load[task_ctx.dom_id as usize].insert(
                    OrderedFloat(this_load),
                    TaskInfo {
                        pid,
                        dom_mask: task_ctx.dom_mask,
                        migrated: Cell::new(false),
                        is_kworker: task_ctx.is_kworker,
                    },
                );
            }
        }

        self.load_avg = load_sum / self.top.nr_doms as f64;
        *self.task_loads = this_task_loads;
        Ok(())
    }

    // To balance dom loads we identify doms with lower and higher load than average
    fn calculate_dom_load_balance(&mut self) -> Result<()> {
        for (dom, dom_load) in self.dom_loads.iter().enumerate() {
            let imbal = dom_load - self.load_avg;
            if imbal.abs() >= self.load_avg * Self::LOAD_IMBAL_HIGH_RATIO {
                if imbal > 0f64 {
                    self.doms_to_push.insert(OrderedFloat(imbal), dom as u32);
                } else {
                    self.doms_to_pull.insert(OrderedFloat(-imbal), dom as u32);
                }
                self.imbal[dom] = imbal;
            }
        }
        Ok(())
    }

    // Find the first candidate pid which hasn't already been migrated and
    // can run in @pull_dom.
    fn find_first_candidate<'d, I>(
        tasks_by_load: I,
        pull_dom: u32,
        skip_kworkers: bool,
    ) -> Option<(f64, &'d TaskInfo)>
    where
        I: IntoIterator<Item = (&'d OrderedFloat<f64>, &'d TaskInfo)>,
    {
        match tasks_by_load
            .into_iter()
            .skip_while(|(_, task)| {
                task.migrated.get()
                    || (task.dom_mask & (1 << pull_dom) == 0)
                    || (skip_kworkers && task.is_kworker)
            })
            .next()
        {
            Some((OrderedFloat(load), task)) => Some((*load, task)),
            None => None,
        }
    }

    fn pick_victim(
        &self,
        (push_dom, to_push): (u32, f64),
        (pull_dom, to_pull): (u32, f64),
    ) -> Option<(&TaskInfo, f64)> {
        let to_xfer = to_pull.min(to_push) * Self::LOAD_IMBAL_XFER_TARGET_RATIO;

        trace!(
            "considering dom {}@{:.2} -> {}@{:.2}",
            push_dom,
            to_push,
            pull_dom,
            to_pull
        );

        let calc_new_imbal = |xfer: f64| (to_push - xfer).abs() + (to_pull - xfer).abs();

        trace!(
            "to_xfer={:.2} tasks_by_load={:?}",
            to_xfer,
            &self.tasks_by_load[push_dom as usize]
        );

        // We want to pick a task to transfer from push_dom to pull_dom to
        // reduce the load imbalance between the two closest to $to_xfer.
        // IOW, pick a task which has the closest load value to $to_xfer
        // that can be migrated. Find such task by locating the first
        // migratable task while scanning left from $to_xfer and the
        // counterpart while scanning right and picking the better of the
        // two.
        let (load, task, new_imbal) = match (
            Self::find_first_candidate(
                self.tasks_by_load[push_dom as usize]
                    .range((Unbounded, Included(&OrderedFloat(to_xfer))))
                    .rev(),
                pull_dom,
                self.skip_kworkers,
            ),
            Self::find_first_candidate(
                self.tasks_by_load[push_dom as usize]
                    .range((Included(&OrderedFloat(to_xfer)), Unbounded)),
                pull_dom,
                self.skip_kworkers,
            ),
        ) {
            (None, None) => return None,
            (Some((load, task)), None) | (None, Some((load, task))) => {
                (load, task, calc_new_imbal(load))
            }
            (Some((load0, task0)), Some((load1, task1))) => {
                let (new_imbal0, new_imbal1) = (calc_new_imbal(load0), calc_new_imbal(load1));
                if new_imbal0 <= new_imbal1 {
                    (load0, task0, new_imbal0)
                } else {
                    (load1, task1, new_imbal1)
                }
            }
        };

        // If the best candidate can't reduce the imbalance, there's nothing
        // to do for this pair.
        let old_imbal = to_push + to_pull;
        if old_imbal < new_imbal {
            trace!(
                "skipping pid {}, dom {} -> {} won't improve imbal {:.2} -> {:.2}",
                task.pid,
                push_dom,
                pull_dom,
                old_imbal,
                new_imbal
            );
            return None;
        }

        trace!(
            "migrating pid {}, dom {} -> {}, imbal={:.2} -> {:.2}",
            task.pid,
            push_dom,
            pull_dom,
            old_imbal,
            new_imbal,
        );

        Some((task, load))
    }

    // Actually execute the load balancing. Concretely this writes pid -> dom
    // entries into the lb_data map for bpf side to consume.
    fn load_balance(&mut self) -> Result<()> {
        clear_map(self.maps.lb_data());

        trace!("imbal={:?}", &self.imbal);
        trace!("doms_to_push={:?}", &self.doms_to_push);
        trace!("doms_to_pull={:?}", &self.doms_to_pull);

        // Push from the most imbalanced to least.
        while let Some((OrderedFloat(mut to_push), push_dom)) = self.doms_to_push.pop_last() {
            let push_max = self.dom_loads[push_dom as usize] * Self::LOAD_IMBAL_PUSH_MAX_RATIO;
            let mut pushed = 0f64;

            // Transfer tasks from push_dom to reduce imbalance.
            loop {
                let last_pushed = pushed;

                // Pull from the most imbalaned to least.
                let mut doms_to_pull = BTreeMap::<_, _>::new();
                std::mem::swap(&mut self.doms_to_pull, &mut doms_to_pull);
                let mut pull_doms = doms_to_pull.into_iter().rev().collect::<Vec<(_, _)>>();

                for (to_pull, pull_dom) in pull_doms.iter_mut() {
                    if let Some((task, load)) =
                        self.pick_victim((push_dom, to_push), (*pull_dom, f64::from(*to_pull)))
                    {
                        // Execute migration.
                        task.migrated.set(true);
                        to_push -= load;
                        *to_pull -= load;
                        pushed += load;

                        // Ask BPF code to execute the migration.
                        let pid = task.pid;
                        let cpid = (pid as libc::pid_t).to_ne_bytes();
                        if let Err(e) = self.maps.lb_data().update(
                            &cpid,
                            &pull_dom.to_ne_bytes(),
                            libbpf_rs::MapFlags::NO_EXIST,
                        ) {
                            warn!(
                                "Failed to update lb_data map for pid={} error={:?}",
                                pid, &e
                            );
                            *self.nr_lb_data_errors += 1;
                        }

                        // Always break after a successful migration so that
                        // the pulling domains are always considered in the
                        // descending imbalance order.
                        break;
                    }
                }

                pull_doms
                    .into_iter()
                    .map(|(k, v)| self.doms_to_pull.insert(k, v))
                    .count();

                // Stop repeating if nothing got transferred or pushed enough.
                if pushed == last_pushed || pushed >= push_max {
                    break;
                }
            }
        }
        Ok(())
    }
}

struct Scheduler<'a> {
    skel: AtroposSkel<'a>,
    struct_ops: Option<libbpf_rs::Link>,

    sched_interval: Duration,
    tune_interval: Duration,
    load_decay_factor: f64,
    balance_load: bool,
    balanced_kworkers: bool,

    top: Arc<Topology>,

    prev_at: Instant,
    prev_total_cpu: MyCpuStat,
    task_loads: BTreeMap<i32, TaskLoad>,

    nr_lb_data_errors: u64,

    tuner: Tuner,
}

impl<'a> Scheduler<'a> {
    fn init(opts: &Opts) -> Result<Self> {
        // Open the BPF prog first for verification.
        let mut skel_builder = AtroposSkelBuilder::default();
        skel_builder.obj_builder.debug(opts.verbose > 0);
        let mut skel = skel_builder.open().context("Failed to open BPF program")?;

        let nr_cpus = libbpf_rs::num_possible_cpus().unwrap();
        if nr_cpus > atropos_sys::MAX_CPUS as usize {
            bail!(
                "nr_cpus ({}) is greater than MAX_CPUS ({})",
                nr_cpus,
                atropos_sys::MAX_CPUS
            );
        }

        // Initialize skel according to @opts.
        let top = Arc::new(if opts.cpumasks.len() > 0 {
            Topology::from_cpumasks(&opts.cpumasks, nr_cpus)?
        } else {
            Topology::from_cache_level(opts.cache_level, nr_cpus)?
        });

        skel.rodata().nr_doms = top.nr_doms as u32;
        skel.rodata().nr_cpus = top.nr_cpus as u32;

        for (cpu, dom) in top.cpu_dom.iter().enumerate() {
            skel.rodata().cpu_dom_id_map[cpu] = dom.unwrap_or(0) as u32;
        }

        for (dom, cpus) in top.dom_cpus.iter().enumerate() {
            let raw_cpus_slice = cpus.as_raw_slice();
            let dom_cpumask_slice = &mut skel.rodata().dom_cpumasks[dom];
            let (left, _) = dom_cpumask_slice.split_at_mut(raw_cpus_slice.len());
            left.clone_from_slice(cpus.as_raw_slice());
            info!(
                "DOM[{:02}] cpumask{} ({} cpus)",
                dom,
                &format_cpumask(dom_cpumask_slice, nr_cpus),
                cpus.count_ones()
            );
        }

        skel.rodata().slice_ns = opts.slice_us * 1000;
        skel.rodata().kthreads_local = opts.kthreads_local;
        skel.rodata().fifo_sched = opts.fifo_sched;
        skel.rodata().switch_partial = opts.partial;
        skel.rodata().greedy_threshold = opts.greedy_threshold;

        // Attach.
        let mut skel = skel.load().context("Failed to load BPF program")?;
        skel.attach().context("Failed to attach BPF program")?;
        let struct_ops = Some(
            skel.maps_mut()
                .atropos()
                .attach_struct_ops()
                .context("Failed to attach atropos struct ops")?,
        );
        info!("Atropos Scheduler Attached");

        // Other stuff.
        let prev_total_cpu = MyProcStat::read()?.total;

        Ok(Self {
            skel,
            struct_ops, // should be held to keep it attached

            sched_interval: Duration::from_secs_f64(opts.interval),
            tune_interval: Duration::from_secs_f64(opts.tune_interval),
            load_decay_factor: opts.load_decay_factor.clamp(0.0, 0.99),
            balance_load: !opts.no_load_balance,
            balanced_kworkers: opts.balanced_kworkers,

            top: top.clone(),

            prev_at: Instant::now(),
            prev_total_cpu,
            task_loads: BTreeMap::new(),

            nr_lb_data_errors: 0,

            tuner: Tuner::new(top, opts)?,
        })
    }

    fn get_cpu_busy(&mut self) -> Result<f64> {
        let total_cpu = MyProcStat::read()?.total;
        let busy = total_cpu.calc_util(&self.prev_total_cpu);
        self.prev_total_cpu = total_cpu;
        Ok(busy)
    }

    fn read_bpf_stats(&mut self) -> Result<Vec<u64>> {
        let mut maps = self.skel.maps_mut();
        let stats_map = maps.stats();
        let mut stats: Vec<u64> = Vec::new();
        let zero_vec = vec![vec![0u8; stats_map.value_size() as usize]; self.top.nr_cpus];

        for stat in 0..atropos_sys::stat_idx_ATROPOS_NR_STATS {
            let cpu_stat_vec = stats_map
                .lookup_percpu(&(stat as u32).to_ne_bytes(), libbpf_rs::MapFlags::ANY)
                .with_context(|| format!("Failed to lookup stat {}", stat))?
                .expect("per-cpu stat should exist");
            let sum = cpu_stat_vec
                .iter()
                .map(|val| {
                    u64::from_ne_bytes(
                        val.as_slice()
                            .try_into()
                            .expect("Invalid value length in stat map"),
                    )
                })
                .sum();
            stats_map
                .update_percpu(
                    &(stat as u32).to_ne_bytes(),
                    &zero_vec,
                    libbpf_rs::MapFlags::ANY,
                )
                .context("Failed to zero stat")?;
            stats.push(sum);
        }
        Ok(stats)
    }

    fn report(
        &mut self,
        stats: &Vec<u64>,
        cpu_busy: f64,
        processing_dur: Duration,
        load_avg: f64,
        dom_loads: &Vec<f64>,
        imbal: &Vec<f64>,
    ) {
        let stat = |idx| stats[idx as usize];
        let total = stat(atropos_sys::stat_idx_ATROPOS_STAT_WAKE_SYNC)
            + stat(atropos_sys::stat_idx_ATROPOS_STAT_PREV_IDLE)
            + stat(atropos_sys::stat_idx_ATROPOS_STAT_GREEDY_IDLE)
            + stat(atropos_sys::stat_idx_ATROPOS_STAT_PINNED)
            + stat(atropos_sys::stat_idx_ATROPOS_STAT_DIRECT_DISPATCH)
            + stat(atropos_sys::stat_idx_ATROPOS_STAT_DIRECT_GREEDY)
            + stat(atropos_sys::stat_idx_ATROPOS_STAT_DIRECT_GREEDY_FAR)
            + stat(atropos_sys::stat_idx_ATROPOS_STAT_DSQ_DISPATCH)
            + stat(atropos_sys::stat_idx_ATROPOS_STAT_GREEDY);

        info!(
            "cpu={:7.2} bal={} load_avg={:8.2} task_err={} lb_data_err={} proc={:?}ms",
            cpu_busy * 100.0,
            stats[atropos_sys::stat_idx_ATROPOS_STAT_LOAD_BALANCE as usize],
            load_avg,
            stats[atropos_sys::stat_idx_ATROPOS_STAT_TASK_GET_ERR as usize],
            self.nr_lb_data_errors,
            processing_dur.as_millis(),
        );

        let stat_pct = |idx| stat(idx) as f64 / total as f64 * 100.0;

        info!(
            "tot={:7} wsync={:5.2} prev_idle={:5.2} greedy_idle={:5.2} pin={:5.2}",
            total,
            stat_pct(atropos_sys::stat_idx_ATROPOS_STAT_WAKE_SYNC),
            stat_pct(atropos_sys::stat_idx_ATROPOS_STAT_PREV_IDLE),
            stat_pct(atropos_sys::stat_idx_ATROPOS_STAT_GREEDY_IDLE),
            stat_pct(atropos_sys::stat_idx_ATROPOS_STAT_PINNED),
        );

        info!(
            "dir={:5.2} dir_greedy={:5.2} dir_greedy_far={:5.2}",
            stat_pct(atropos_sys::stat_idx_ATROPOS_STAT_DIRECT_DISPATCH),
            stat_pct(atropos_sys::stat_idx_ATROPOS_STAT_DIRECT_GREEDY),
            stat_pct(atropos_sys::stat_idx_ATROPOS_STAT_DIRECT_GREEDY_FAR),
        );

        info!(
            "dsq={:5.2} greedy={:5.2} kick_greedy={:5.2} rep={:5.2}",
            stat_pct(atropos_sys::stat_idx_ATROPOS_STAT_DSQ_DISPATCH),
            stat_pct(atropos_sys::stat_idx_ATROPOS_STAT_GREEDY),
            stat_pct(atropos_sys::stat_idx_ATROPOS_STAT_KICK_GREEDY),
            stat_pct(atropos_sys::stat_idx_ATROPOS_STAT_REPATRIATE),
        );

        let ti = &self.skel.bss().tune_input;
        info!(
            "direct_greedy_cpumask={}",
            format_cpumask(&ti.direct_greedy_cpumask, self.top.nr_cpus)
        );
        info!(
            "  kick_greedy_cpumask={}",
            format_cpumask(&ti.kick_greedy_cpumask, self.top.nr_cpus)
        );

        for i in 0..self.top.nr_doms {
            info!(
                "DOM[{:02}] util={:6.2} load={:8.2} imbal={}",
                i,
                self.tuner.dom_utils[i] * 100.0,
                dom_loads[i],
                if imbal[i] == 0.0 {
                    format!("{:9.2}", 0.0)
                } else {
                    format!("{:+9.2}", imbal[i])
                },
            );
        }
    }

    fn lb_step(&mut self) -> Result<()> {
        let started_at = Instant::now();
        let bpf_stats = self.read_bpf_stats()?;
        let cpu_busy = self.get_cpu_busy()?;

        let mut lb = LoadBalancer::new(
            self.skel.maps_mut(),
            self.top.clone(),
            &mut self.task_loads,
            self.load_decay_factor,
            self.balanced_kworkers,
            &mut self.nr_lb_data_errors,
        );

        lb.read_task_loads(started_at.duration_since(self.prev_at))?;
        lb.calculate_dom_load_balance()?;

        if self.balance_load {
            lb.load_balance()?;
        }

        // Extract fields needed for reporting and drop lb to release
        // mutable borrows.
        let (load_avg, dom_loads, imbal) = (lb.load_avg, lb.dom_loads, lb.imbal);

        self.report(
            &bpf_stats,
            cpu_busy,
            Instant::now().duration_since(started_at),
            load_avg,
            &dom_loads,
            &imbal,
        );

        self.prev_at = started_at;
        Ok(())
    }

    fn read_bpf_exit_type(&mut self) -> i32 {
        unsafe { std::ptr::read_volatile(&self.skel.bss().exit_type as *const _) }
    }

    fn report_bpf_exit_type(&mut self) -> Result<()> {
        // Report msg if EXT_OPS_EXIT_ERROR.
        match self.read_bpf_exit_type() {
            0 => Ok(()),
            etype if etype == 2 => {
                let cstr = unsafe { CStr::from_ptr(self.skel.bss().exit_msg.as_ptr() as *const _) };
                let msg = cstr
                    .to_str()
                    .context("Failed to convert exit msg to string")
                    .unwrap();
                bail!("BPF exit_type={} msg={}", etype, msg);
            }
            etype => {
                info!("BPF exit_type={}", etype);
                Ok(())
            }
        }
    }

    fn run(&mut self, shutdown: Arc<AtomicBool>) -> Result<()> {
        let now = Instant::now();
        let mut next_tune_at = now + self.tune_interval;
        let mut next_sched_at = now + self.sched_interval;

        while !shutdown.load(Ordering::Relaxed) && self.read_bpf_exit_type() == 0 {
            let now = Instant::now();

            if now >= next_tune_at {
                self.tuner.step(&mut self.skel)?;
                next_tune_at += self.tune_interval;
                if next_tune_at < now {
                    next_tune_at = now + self.tune_interval;
                }
            }

            if now >= next_sched_at {
                self.lb_step()?;
                next_sched_at += self.sched_interval;
                if next_sched_at < now {
                    next_sched_at = now + self.sched_interval;
                }
            }

            std::thread::sleep(
                next_sched_at
                    .min(next_tune_at)
                    .duration_since(Instant::now()),
            );
        }

        self.report_bpf_exit_type()
    }
}

impl<'a> Drop for Scheduler<'a> {
    fn drop(&mut self) {
        if let Some(struct_ops) = self.struct_ops.take() {
            drop(struct_ops);
        }
    }
}

fn main() -> Result<()> {
    let opts = Opts::parse();

    let llv = match opts.verbose {
        0 => simplelog::LevelFilter::Info,
        1 => simplelog::LevelFilter::Debug,
        _ => simplelog::LevelFilter::Trace,
    };
    let mut lcfg = simplelog::ConfigBuilder::new();
    lcfg.set_time_level(simplelog::LevelFilter::Error)
        .set_location_level(simplelog::LevelFilter::Off)
        .set_target_level(simplelog::LevelFilter::Off)
        .set_thread_level(simplelog::LevelFilter::Off);
    simplelog::TermLogger::init(
        llv,
        lcfg.build(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    )?;

    let mut sched = Scheduler::init(&opts)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    ctrlc::set_handler(move || {
        shutdown_clone.store(true, Ordering::Relaxed);
    })
    .context("Error setting Ctrl-C handler")?;

    sched.run(shutdown)
}
