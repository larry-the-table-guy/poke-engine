use core::convert::TryInto;
use core::hash::BuildHasher;
use core::num::NonZeroU32;
use core::time::Duration;
use std::io::{stdout, IsTerminal, Read, Write};

use poke_engine::instruction;
use poke_engine::state::State;
use poke_engine::{mcts, mcts_threaded};
use profiling::TreeStats;

use foldhash::quality as fhash;

use profiling::{HistList, Histogram, SampleStats};

mod profiling;

fn main() {
    let args = std::env::args()
        .skip(1)
        .filter(|s| s != "--bench") // ignore, passed by cargo
        .collect::<Vec<String>>();

    let (command, args) = args.split_first().expect("need at least one arg for mode");

    // parse --key=value flags
    let mut stats_type = StatsType::Full;
    let mut output_mode = ReportBackend::Markdown;
    let mut max_time = Duration::from_secs(5);
    let mut num_threads: Option<NonZeroU32> = None; // default to single threaded
    for arg in args {
        if let Some(s) = arg.strip_prefix("--stats=") {
            stats_type = match s {
                "full" => StatsType::Full,
                "short" => StatsType::Short,
                "none" => StatsType::None,
                other => panic!("'{}' is not a valid arg for --stats=...", other),
            };
        } else if let Some(mode) = arg.strip_prefix("--output=") {
            output_mode = match mode {
                "binary" => {
                    if std::io::stdout().is_terminal() {
                        panic!("Hey! You asked for binary output, but stdout is a terminal. Redirect stdout.");
                    }
                    ReportBackend::Binary
                }
                "markdown" => ReportBackend::Markdown,
                "python" => ReportBackend::Python,
                other => {
                    panic!("This program doesn't support '{}' as an output mode", other)
                }
            };
        } else if let Some(seconds) = arg.strip_prefix("--time=") {
            max_time = Duration::from_secs(seconds.parse().unwrap());
        } else if let Some(count) = arg.strip_prefix("--threads=") {
            let count = count.parse::<u32>().unwrap();
            num_threads = NonZeroU32::new(count);
        } else if arg.starts_with("--") {
            panic!("unrecognized argument '{}'", arg)
        } else {
            // ignore, handled by each separate command
        }
    }

    match command.as_str() {
        "bench" => {
            bench_mcts(num_threads, stats_type, output_mode, max_time);
        }
        "diff" => {
            let files = args;
            let mut buf = Vec::<u8>::new();
            let reports = files
                .iter()
                .map(|s| {
                    buf.clear();
                    let p = std::path::Path::new(&s);
                    let mut file = std::fs::OpenOptions::new().read(true).open(p).unwrap();
                    file.read_to_end(&mut buf).unwrap();
                    let (a, b, c) = binary_deserialize(buf.as_slice());
                    (s.as_str(), a, b, c)
                })
                .collect::<Vec<_>>();
            let reports = reports
                .iter()
                .map(|r| (r.0, &r.1, r.2.as_slice(), &r.3))
                .collect::<Vec<_>>();
            diff(reports.as_slice());
        }
        "print" => {
            let mut stdin = std::io::stdin().lock();
            let mut buf = Vec::with_capacity(0);
            stdin.read_to_end(&mut buf).unwrap();
            let (header, sample_stats, ts) = binary_deserialize(buf.as_slice());
            output(output_mode, stats_type, &header, &sample_stats, &ts);
        }
        s if s.starts_with("--") => {
            panic!("missing mode argument");
        }
        s => panic!("unrecognized mode '{}'", s),
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum ReportBackend {
    Markdown,
    Binary,
    Python,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum StatsType {
    Full,
    Short,
    None,
}

fn bench_mcts(
    num_threads: Option<NonZeroU32>,
    stats_type: StatsType,
    output_mode: ReportBackend,
    max_time: Duration,
) {
    let mut sample_stats = Vec::<SampleStats>::with_capacity(1);
    let mut tree_stats = TreeStats::new();

    for line in std::io::stdin()
        .lines()
        .map(|r| r.unwrap())
        .filter(|l| !l.is_empty())
        .filter(|l| !l.starts_with('#'))
    {
        let hash = hash_state_string(&line);
        let mut state = State::deserialize(&line);
        let start = std::time::Instant::now();
        let (s1_options, s2_options) = state.root_get_all_options();
        let mut proc_mem_usage = None;

        let (iter_count, seconds, sub_timers) = if let Some(num_threads) = num_threads {
            let (r, _root, timers, childmap) = mcts_threaded::perform_mcts_shared_tree_inner(
                &mut state,
                s1_options,
                s2_options,
                max_time,
                num_threads.get() as usize,
            );
            if stats_type != StatsType::None {
                proc_mem_usage = memory_stats::memory_stats();
            }
            let seconds = start.elapsed().as_secs_f64();
            if stats_type == StatsType::Full {
                tree_stats.analyze_threaded(&childmap);
            }
            (r.iteration_count, seconds, timers)
        } else {
            let (r, _root, timers, childmap) =
                mcts::perform_mcts_inner(&mut state, s1_options, s2_options, max_time);
            if stats_type != StatsType::None {
                proc_mem_usage = memory_stats::memory_stats();
            }
            let seconds = start.elapsed().as_secs_f64();
            if stats_type == StatsType::Full {
                tree_stats.analyze(&childmap);
            }
            (r.iteration_count, seconds, timers)
        };
        let (phys, virt) = proc_mem_usage
            .map(|p| (p.physical_mem, p.virtual_mem))
            .unwrap_or_default();

        sample_stats.push(SampleStats {
            state_hash: hash,
            phys_mem_usage: phys as u64,
            virt_mem_usage: virt as u64,
            iter_count: iter_count as u64,
            seconds,
            sub_timers,
        });
    }

    if sample_stats.is_empty() {
        // no samples, just exit w/out any output
        return;
    }
    let elem_sizes = if num_threads.is_none() {
        ElemSizes::CURRENT
    } else {
        ElemSizes::CURRENT_THREADED
    };
    let mut header = BinHeader {
        version: BinHeader::CURRENT_VERSION,
        header_checksum: 0,
        num_threads,
        num_samples: sample_stats
            .len()
            .try_into()
            .expect("fewer than 4 billion samples"),
        cmap_len: tree_stats.map_len as u64,
        cmap_cap: tree_stats.map_cap as u64,
        elem_sizes: elem_sizes.clone(),
    };
    header.update_checksum();

    output(output_mode, stats_type, &header, &sample_stats, &tree_stats);
}

fn output(
    output_mode: ReportBackend,
    stats_type: StatsType,
    header: &BinHeader,
    sample_stats: &[SampleStats],
    tree_stats: &TreeStats,
) {
    match (output_mode, stats_type) {
        (_, StatsType::None) => return,
        (ReportBackend::Markdown, stats_type) => {
            pretty_print(stats_type, &header, &sample_stats, &tree_stats);
        }
        (ReportBackend::Binary, StatsType::Full) => {
            binary_serialize(&header, &sample_stats, &tree_stats);
        }
        (ReportBackend::Binary | ReportBackend::Python, StatsType::Short) => {
            // (Lowest priority use case)
            // Just CSV of iter stats.
            // I don't see a real need for a binary format for just iter stats.
            println!("state hash,phys mem (B),virt mem (B),iter count,time (seconds)");
            for SampleStats {
                state_hash,
                phys_mem_usage,
                virt_mem_usage,
                iter_count,
                seconds,
                sub_timers,
            } in sample_stats
            {
                println!(
                    "{state_hash:x},{phys_mem_usage},{virt_mem_usage},{iter_count},{seconds:.2},{},{},{},{}",
                    sub_timers.selection,
                    sub_timers.expand,
                    sub_timers.rollout,
                    sub_timers.backpropagate,
                )
            }
        }
        (ReportBackend::Python, StatsType::Full) => {
            // FIXME: refine to reduce need for cleanup

            // dictionary on each line. Debug print gets close but has Type names. just want field names
            println!("{:?}", header);
            println!("{:?}", sample_stats);
            println!("{:?}", tree_stats);
        }
    }
}

fn hash_state_string(s: &str) -> u64 {
    fhash::FixedState::default().hash_one(s)
}

/// Fixed-size data for a binary bench report
#[repr(C)]
#[derive(PartialEq, Hash, Debug)]
struct BinHeader {
    /// Tracks binary repr. Print and Diff formats are free to change.
    version: u32,
    /// For detecting header repr changes. Set to zero when calculating.
    ///
    /// More foolproof than version, but less descriptive for debugging.
    header_checksum: u32,
    /// None for unthreaded
    num_threads: Option<NonZeroU32>,
    num_samples: u32,
    cmap_len: u64,
    cmap_cap: u64,
    // Recorded in header so that we can still compare data as the repr changes
    elem_sizes: ElemSizes,
}
impl BinHeader {
    pub const CURRENT_VERSION: u32 = 1;
    pub fn update_checksum(&mut self) -> u32 {
        self.header_checksum = 0;
        self.header_checksum = fhash::FixedState::default().hash_one(&self) as u32;
        self.header_checksum
    }
}

/// Snapshot of relevant data sizes
#[repr(C)]
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct ElemSizes {
    /// Entry of ChildMap
    child_map_kv: u32,
    /// Node (plus wrappers in backing storage. E.g., Arc in mcts_threaded
    node: u32,
    move_node: u32,
    instruction: u32,
}
impl ElemSizes {
    // NOTE: These need to be kept up to date as the implementations change.
    // If indirection is added or removed, account for it.
    pub const CURRENT: Self = Self {
        // Hashbrown buckets store (K,V)
        child_map_kv: size_of::<(mcts::ChildMapK, mcts::ChildMapV)>() as u32,
        node: size_of::<mcts::Node>() as u32,
        move_node: size_of::<mcts::MoveNode>() as u32,
        instruction: size_of::<instruction::Instruction>() as u32,
    };
    pub const CURRENT_THREADED: Self = Self {
        child_map_kv: size_of::<(mcts_threaded::ChildMapK, mcts_threaded::ChildMapV)>() as u32,
        // See [poke_engine::mcts_threaded::SharedBranch]
        // Arc in backing Vec, ArcInner -> 2 usizes
        node: (size_of::<std::sync::Arc<mcts_threaded::Node>>()
            + size_of::<(usize, usize, mcts_threaded::Node)>()) as u32,
        move_node: size_of::<mcts_threaded::MoveNode>() as u32,
        instruction: size_of::<instruction::Instruction>() as u32,
    };
}

// TODO: maybe extend diff and remove this
// adding more props to diff isn't that hard, less total code.
// just a question of presentation
fn pretty_print(
    stats_type: StatsType,
    header: &BinHeader,
    sample_stats: &[SampleStats],
    tree_stats: &TreeStats,
) {
    println!("# Samples Summary\n");
    if let Some(count) = header.num_threads {
        println!("multi-threaded: {count}")
    } else {
        println!("single-threaded")
    }
    println!("\n");
    println!(
        "| {:^16} | {:^11} | {:^12} | {:^13} | {:^13} | {:^13} |",
        "State Hash", "# Iters", "Time (s)", "Iters / sec", "Phys Mem MB", "Virt Mem MB"
    );
    println!(
        "|{}:|{}:|{}:|{}:|{}:|{}:|",
        "-".repeat(17),
        "-".repeat(12),
        "-".repeat(13),
        "-".repeat(14),
        "-".repeat(14),
        "-".repeat(14),
    );
    for stats in sample_stats {
        println!(
            "| {:16x} | {:11} | {:12.2} | {:13.2} | {:13.2} | {:13.2} |",
            stats.state_hash,
            stats.iter_count,
            stats.seconds,
            stats.iter_count as f64 / stats.seconds,
            stats.phys_mem_usage as f64 / 1_000_000.,
            stats.virt_mem_usage as f64 / 1_000_000.,
        );
    }

    if stats_type != StatsType::Full {
        return;
    }
    println!("\n");
    println!("# Memory Breakdown\n");
    // TODO: review where to use virt or phys for total % quotient

    let num_samples_with_proc_data = sample_stats
        .iter()
        .filter(|s| s.phys_mem_usage != 0)
        .count();
    let total_proc_used = sample_stats.iter().map(|s| s.phys_mem_usage).sum::<u64>();
    let total_proc_reserved = sample_stats.iter().map(|s| s.virt_mem_usage).sum::<u64>();
    if total_proc_used == 0 {
        eprintln!("Process memory usage stats absent, omitting");
    } else if num_samples_with_proc_data != sample_stats.len() {
        eprintln!("1 or more samples are missing process memory usage data, stats may be skewed");
    }
    let avg_proc_mb_used = total_proc_used as f64 / 1_000_000. / num_samples_with_proc_data as f64;
    let avg_proc_mb_reserved =
        total_proc_reserved as f64 / 1_000_000. / num_samples_with_proc_data as f64;

    let table_stuff = [
        (
            "child_map::(K,V)",
            header.elem_sizes.child_map_kv,
            tree_stats.map_len,
            tree_stats.map_cap,
        ),
        (
            "Node",
            header.elem_sizes.node,
            tree_stats.hists.node_len.weighted_sum(),
            tree_stats.hists.node_cap.weighted_sum(),
        ),
        (
            "MoveNode",
            header.elem_sizes.move_node,
            tree_stats.hists.move_node_len.weighted_sum(),
            tree_stats.hists.move_node_cap.weighted_sum(),
        ),
        (
            "Instruction",
            header.elem_sizes.instruction,
            tree_stats.hists.instr_list_len.weighted_sum(),
            tree_stats.hists.instr_list_cap.weighted_sum(),
        ),
    ];

    println!(
        "| {:^20} | {:^8} | {:^12} | {:^13} | {:^12} | {:^13} | {:^9} | {:^9} |",
        "Object",
        "Size",
        "Num used",
        "MB used",
        "Num reserved",
        "MB reserved",
        "% Spare",
        "% Total"
    );
    println!(
        "|:{}|{}:|{}:|{}:|{}:|{}:|{}:|{}:|",
        "-".repeat(21),
        "-".repeat(9),
        "-".repeat(13),
        "-".repeat(14),
        "-".repeat(13),
        "-".repeat(14),
        "-".repeat(10),
        "-".repeat(10),
    );

    let (sum_mb_used, sum_mb_reserved) =
        table_stuff
            .iter()
            .fold((0, 0), |(uacc, racc), (_, size, used, reserved)| {
                (
                    uacc + used * *size as usize,
                    racc + reserved * *size as usize,
                )
            });
    let sum_mb_used = sum_mb_used as f64 / 1_000_000. / sample_stats.len() as f64;
    let sum_mb_reserved = sum_mb_reserved as f64 / 1_000_000. / sample_stats.len() as f64;
    let avg_mb_used = avg_proc_mb_used.max(sum_mb_used);
    let avg_mb_reserved = avg_proc_mb_reserved.max(sum_mb_reserved);
    let pct_total_spare = 100. * (avg_mb_reserved - avg_mb_used) / avg_mb_reserved;

    for (name, size, used, reserved) in table_stuff {
        let used = used / sample_stats.len();
        let reserved = reserved / sample_stats.len();
        assert!(
            used <= reserved,
            "faulty data, capacity shouldn't be less than len"
        );
        let used_mb = (used * size as usize) as f64 / 1_000_000f64;
        let reserved_mb = (reserved * size as usize) as f64 / 1_000_000f64;
        let pct_spare = (1f64 - used as f64 / reserved as f64) * 100f64;
        let pct_total = 100f64 * reserved_mb as f64 / avg_mb_reserved as f64;
        println!(
            "| {name:20} | {size:8} | {used:12} | {used_mb:13.2} | {reserved:12} | {reserved_mb:13.2} | {pct_spare:9.2} | {pct_total:9.2} |"
        );
    }
    println!(
        "|{}|{}|{}|{}|{}|{}|{}|{}|",
        "-".repeat(22),
        "-".repeat(10),
        "-".repeat(14),
        "-".repeat(15),
        "-".repeat(14),
        "-".repeat(15),
        "-".repeat(11),
        "-".repeat(11),
    );
    println!(
        "| {:20} | {:8} | {:12} | {:13.2} | {:12} | {:13.2} | {:9.2} | {:9.2} |",
        "Subtotals",
        "",
        "",
        sum_mb_used,
        "",
        sum_mb_reserved,
        100. * (sum_mb_reserved - sum_mb_used) / sum_mb_reserved,
        100. * sum_mb_reserved / avg_mb_used
    );
    // sum_mb_reserved maps more closely to phys mem than virt mem, so makes more sense to use that
    // as the total

    let unacc_mb_used = avg_mb_used - sum_mb_reserved;
    println!(
        "| {:20} | {:8} | {:12} | {:13.2} | {:12} | {:13.2} | {:9.2} | {:9.2} |",
        "Unaccounted",
        "",
        "",
        unacc_mb_used,
        "",
        "",
        "",
        100f64 * unacc_mb_used / avg_mb_used
    );
    println!(
        "|{}|{}|{}|{}|{}|{}|{}|{}|",
        "-".repeat(22),
        "-".repeat(10),
        "-".repeat(14),
        "-".repeat(15),
        "-".repeat(14),
        "-".repeat(15),
        "-".repeat(11),
        "-".repeat(11),
    );
    println!(
        "| {:20} | {:8} | {:12} | {:13.2} | {:12} | {:13.2} | {:9.2} | {:9} |",
        "Grand Total", "", "", avg_mb_used, "", avg_mb_reserved, pct_total_spare, 100f64
    );

    // println!("\n");
    // println!("# Time Breakdown\n");

    println!("\n\n# Histograms\n");

    render_hists(&[("", &header, sample_stats, tree_stats)]);
}

type Report<'a> = (&'a str, &'a BinHeader, &'a [SampleStats], &'a TreeStats);

fn diff(reports: &[Report]) {
    let Some((baseline, others)) = reports.split_first() else {
        return;
    };
    if others.len() != 1 {
        println!("# State hashes\n");
        println!(
            "'{}' (Baseline) has {} states",
            baseline.0, baseline.1.num_samples
        );
        for other in others {
            let num_matching_state_hashes = baseline
                .2
                .iter()
                .filter(|b| other.2.iter().any(|o| b.state_hash == o.state_hash))
                .count();
            println!(
                "'{}': {num_matching_state_hashes} / {} states in common with baseline",
                other.0,
                other.2.len()
            );
        }
    }
    println!("\n");

    println!("# Summary\n");

    print!("| {:^25} | {:^14} |", "Property", "Baseline",);
    for other in others {
        print!(
            " {:^14.14} | {:^14} | {:^14} |",
            other.0, "Change", "% Change"
        );
    }
    println!();
    print!("|:{}|{}:|", "-".repeat(26), "-".repeat(15));
    for _ in others {
        print!(
            "{}:|{}:|{}:|",
            "-".repeat(15),
            "-".repeat(15),
            "-".repeat(15)
        );
    }
    println!();

    // TODO: refactor: everything histogram. extract common
    // T1: group of histograms, show pct total
    // T2: other histograms, normalized by iter|sample|thread
    // T3: some may want standard deviation or other

    // TODO: maybe a tag on each property so they can be filtered by diff flags
    fn prop_fn(r: &Report) -> [(&'static str, f64); 23] {
        let bh = &r.1;
        let es = &bh.elem_sizes;
        let ss = &r.2;
        let ts = &r.3;
        let total_iters = ss.iter().map(|s| s.iter_count as usize).sum::<usize>();
        let total_bytes_used = es.node as usize * ts.hists.node_len.weighted_sum()
            + es.move_node as usize * ts.hists.move_node_len.weighted_sum()
            + es.instruction as usize * ts.hists.instr_list_len.weighted_sum()
            + es.child_map_kv as usize * ts.map_len;
        let total_bytes_reserved = es.node as usize * ts.hists.node_cap.weighted_sum()
            + es.move_node as usize * ts.hists.move_node_cap.weighted_sum()
            + es.instruction as usize * ts.hists.instr_list_cap.weighted_sum()
            + es.child_map_kv as usize * ts.map_cap;
        let total_time = ss.iter().map(|s| s.seconds).sum::<f64>();
        let pct_spare_bytes =
            100f64 * (1f64 - total_bytes_used as f64 / total_bytes_reserved as f64);
        let num_samplesf = bh.num_samples as f64;
        let total_itersf = total_iters as f64;
        let num_nodes = ts.hists.node_len.weighted_sum();
        let n_threads = r.1.num_threads.map(NonZeroU32::get).unwrap_or(1);
        // total subtimes across samples, in seconds
        let tot_selection =
            ss.iter().map(|ss| ss.sub_timers.selection).sum::<u64>() as f64 / 1_000_000_000.;
        let tot_expand =
            ss.iter().map(|ss| ss.sub_timers.expand).sum::<u64>() as f64 / 1_000_000_000.;
        let tot_rollout =
            ss.iter().map(|ss| ss.sub_timers.rollout).sum::<u64>() as f64 / 1_000_000_000.;
        let tot_backpropagate =
            ss.iter().map(|ss| ss.sub_timers.backpropagate).sum::<u64>() as f64 / 1_000_000_000.;
        let tot_unaccounted_time =
            total_time - tot_selection - tot_expand - tot_rollout - tot_backpropagate;
        // TODO: moar properties
        #[rustfmt::skip]
        let a = [
            ("Version", bh.version as f64),
            ("Threads", bh.num_threads.map(NonZeroU32::get).unwrap_or_default() as f64),
            ("size(ChildMapKV)", es.child_map_kv as f64),
            ("size(Node)", es.node as f64),
            ("size(MoveNode)", es.move_node as f64),
            ("size(Instruction)", es.instruction as f64),
            ("avg # nodes / sample", num_nodes as f64 / num_samplesf),
            ("avg ChildMap.len / sample", bh.cmap_len as f64 / num_samplesf),
            ("avg ChildMap.cap / sample", bh.cmap_cap as f64 / num_samplesf),
            ("avg MB used / sample", total_bytes_used as f64 / 1_000_000. / num_samplesf),
            ("avg MB reserved / sample", total_bytes_reserved as f64 / 1_000_000. / num_samplesf),
            ("avg B used / iter", total_bytes_used as f64 / total_itersf),
            ("avg B reserved / iter", total_bytes_reserved as f64 / total_itersf),
            ("% spare bytes", pct_spare_bytes),
            ("avg iters / sample", total_itersf / num_samplesf),
            ("avg time (s) / sample", total_time / num_samplesf),
            ("avg iters / sec", total_itersf / total_time),
            ("avg iters / sec / thread", total_itersf / total_time / n_threads as f64),
            ("% time in selection", 100. * tot_selection / total_time),
            ("% time in expand", 100. * tot_expand / total_time),
            ("% time in rollout", 100. * tot_rollout / total_time),
            ("% time in backpropagate", 100. * tot_backpropagate / total_time),
            ("% time unaccounted", 100. * tot_unaccounted_time / total_time),
        ];
        a
    }
    let baseline_props = prop_fn(baseline);
    let other_props = others
        .iter()
        .map(|v| prop_fn(v).map(|t| t.1))
        .collect::<Vec<_>>();
    for (prop_idx, (name, baseline_v)) in baseline_props.iter().enumerate() {
        print!("| {:25} | {:14.2} |", name, baseline_v,);
        for o_v in other_props.iter() {
            let o_v = o_v[prop_idx];
            let change = o_v - baseline_v;
            let pct_change = 100f64 * (change / baseline_v);
            // TODO: maybe an option to leave change column blank when identical
            print!(" {:14.2} | {:+14.2} | {:+14.2} |", o_v, change, pct_change);
        }
        println!();
    }
    println!();

    println!("# Histograms\n");

    render_hists(reports);
}

fn render_hists(reports: &[Report]) {
    let Some((baseline, others)) = reports.split_first() else {
        return;
    };
    for hist_idx in 0..HistList::COUNT {
        let hist_name = HistList::LABELS[hist_idx];
        println!("## {hist_name}\n");

        print!("|    Key    |");
        for idx in 0..reports.len() {
            print!(
                " {:^8} | % Total | %PSum/T | %C/SSum |",
                if reports.len() == 1 {
                    "Count".to_owned()
                } else {
                    format!("Report {idx}")
                }
            );
        }
        println!();
        print!("|{}:|", "-".repeat(10));
        for _ in reports {
            print!(
                "{}:|{}:|{}:|{}:|",
                "-".repeat(9),
                "-".repeat(8),
                "-".repeat(8),
                "-".repeat(8)
            );
        }
        println!();

        let mut iters = reports
            .iter()
            .map(|r| r.3.hists.as_array()[hist_idx].stat_iter().peekable())
            .collect::<Vec<_>>();
        while let Some(k) = iters.iter_mut().filter_map(|i| i.peek()).map(|s| s.0).min() {
            print!("| {k:9} |");
            for iterator in iters.iter_mut() {
                if let Some((_, count, pct_total, pct_psum, pct_ssum)) =
                    iterator.next_if(|r| r.0 == k)
                {
                    print!("{count:9} | {pct_total:7.2} | {pct_psum:7.2} | {pct_ssum:7.2} |");
                } else {
                    print!("{:9} | {:7} | {:7} | {:7} |", "", "", "", "");
                }
            }
            println!();
        }
        println!();

        // TODO: stuff like min, max, percentiles
        // scrap this? don't have any props in mind that are non-trivial
        // how to handle empty hists? just blank column?
        if false {
            println!("### Summary Stats\n");
            print!("| {:^25} | {:^10} |", "Property", "Baseline",);
            for o in others {
                print!("{:^16.16}| {:^8} | {:^8} |", o.0, "Change", "% Change");
            }
            println!();
            print!("|{}:|{}:|", "-".repeat(26), "-".repeat(11));
            for _ in others {
                print!("{}:|{}:|{}:|", "-".repeat(15), "-".repeat(9), "-".repeat(9));
            }
            println!();

            let hist_props: &[(&str, fn(&Histogram) -> f64)] = &[
                ("min", |h| h.iter().next().map(|(k, _)| k as f64).unwrap()),
                ("max", |h| h.iter().last().map(|(k, _)| k as f64).unwrap()),
                // percentiles?
            ];
            for (name, fnc) in hist_props {
                let baseline_hist = &baseline.3.hists.as_array()[hist_idx];
                let baseline_v = fnc(baseline_hist);
                print!("| {:25} | {:10} |", name, baseline_v);
                for o in others {
                    let o_v = fnc(&o.3.hists.as_array()[hist_idx]);
                    let change = baseline_v - o_v;
                    let pct_change = change / baseline_v;
                    print!(" {o_v:14} | {change:+8.2} | {pct_change:+8.2} |");
                }
                println!();
            }
            println!();
        }
    }
}

fn binary_serialize(header: &BinHeader, sample_stats: &[SampleStats], ts: &TreeStats) {
    let mut scratch_buf = Vec::<u8>::with_capacity(4096 * 4);

    scratch_buf.extend_from_slice(unsafe {
        core::slice::from_raw_parts(
            core::ptr::from_ref(header).cast::<u8>(),
            size_of::<BinHeader>(),
        )
    });

    scratch_buf.extend_from_slice(unsafe {
        let len = size_of_val(sample_stats);
        core::slice::from_raw_parts(sample_stats.as_ptr().cast::<u8>(), len)
    });

    for hist in ts.hists.as_array() {
        hist.serialize(&mut scratch_buf);
    }

    stdout().write_all(&scratch_buf).unwrap();
    stdout().flush().unwrap();
}

// TODO: replace panic with Result, I guess
fn binary_deserialize(data: &[u8]) -> (BinHeader, Vec<SampleStats>, TreeStats) {
    let (header, rest) = data.split_at(size_of::<BinHeader>());
    let mut header = unsafe { header.as_ptr().cast::<BinHeader>().read_unaligned() };
    let recorded_header_checksum = header.header_checksum;
    let actual_header_checksum = header.update_checksum();
    assert_eq!(
        BinHeader::CURRENT_VERSION,
        header.version,
        "unsupported report version (maybe a pretty print report)",
    );
    assert_eq!(
        recorded_header_checksum, actual_header_checksum,
        "header checksum mismatch.",
    );

    let (sample_stats, rest) =
        rest.split_at(header.num_samples as usize * size_of::<SampleStats>());
    let sample_stats = unsafe {
        let mut vec = Vec::<SampleStats>::with_capacity(header.num_samples as usize);
        core::ptr::copy_nonoverlapping(
            sample_stats.as_ptr(),
            vec.as_mut_ptr().cast::<u8>(),
            sample_stats.len(),
        );
        vec.set_len(header.num_samples as usize);
        vec
    };

    let mut ts = TreeStats::new();
    ts.map_len = header.cmap_len as usize;
    ts.map_cap = header.cmap_cap as usize;
    let mut rest = rest;
    for hist in ts.hists.as_array_mut() {
        rest = hist.deserialize(rest).unwrap();
    }
    (header, sample_stats, ts)
}
