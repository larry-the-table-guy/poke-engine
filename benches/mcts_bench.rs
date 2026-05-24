use core::hash::BuildHasher;
use core::num::NonZeroU32;
use core::time::Duration;
use std::io::{stdout, IsTerminal, Read, Write};

use poke_engine::instruction;
use poke_engine::state::State;
use poke_engine::{mcts, mcts_threaded};

use foldhash::quality as fhash;

use profiling::{DisplayType, HistList};

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
                    let (a, b) = binary_deserialize(buf.as_slice());
                    (s.as_str(), a, b)
                })
                .collect::<Vec<_>>();
            let reports = reports
                .iter()
                .map(|r| (r.0, &r.1, &r.2))
                .collect::<Vec<_>>();
            diff(reports.as_slice());
        }
        // TODO: remove, replace with diff
        "print" => {
            let mut stdin = std::io::stdin().lock();
            let mut buf = Vec::with_capacity(0);
            stdin.read_to_end(&mut buf).unwrap();
            let (header, stats) = binary_deserialize(buf.as_slice());
            output(output_mode, stats_type, &header, &stats);
        }
        "merge" => {
            // take file paths from args, compare version numbers, spit out report
            todo!("");
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
    let mut stats = HistList::new();
    let mut at_least_one = false;

    for line in std::io::stdin()
        .lines()
        .map(|r| r.unwrap())
        .filter(|l| !l.is_empty())
        .filter(|l| !l.starts_with('#'))
    {
        at_least_one = true;
        let hash = hash_state_string(&line);
        let mut state = State::deserialize(&line);
        let start = std::time::Instant::now();
        let (s1_options, s2_options) = state.root_get_all_options();
        let mut proc_mem_usage = None;

        let (iter_count, time_ms, sub_timers) = if let Some(num_threads) = num_threads {
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
            let time_ms = start.elapsed().as_millis() as u64;
            if stats_type == StatsType::Full {
                stats.analyze_threaded_tree(&childmap);
            }
            (r.iteration_count, time_ms, timers)
        } else {
            let (r, _root, timers, childmap) =
                mcts::perform_mcts_inner(&mut state, s1_options, s2_options, max_time);
            if stats_type != StatsType::None {
                proc_mem_usage = memory_stats::memory_stats();
            }
            let time_ms = start.elapsed().as_millis() as u64;
            if stats_type == StatsType::Full {
                stats.analyze_tree(&childmap);
            }
            (r.iteration_count, time_ms, timers)
        };

        stats.state_hash.inc(hash);
        if let Some(mem) = proc_mem_usage {
            stats.phys_mem_usage.inc(mem.physical_mem as u64);
            stats.virt_mem_usage.inc(mem.virtual_mem as u64);
        }
        stats.iter_count.inc(iter_count as u64);
        stats.total_ms.inc(time_ms);
        stats.analyze_time(sub_timers);
    }

    if !at_least_one {
        // no samples, just exit w/out any output
        return;
    }
    let elem_sizes = if num_threads.is_none() {
        ElemSizes::CURRENT
    } else {
        ElemSizes::CURRENT_THREADED
    };
    let header = BinHeader::new(num_threads, elem_sizes.clone());

    output(output_mode, stats_type, &header, &stats);
}

fn output(output_mode: ReportBackend, stats_type: StatsType, header: &BinHeader, stats: &HistList) {
    match (output_mode, stats_type) {
        (_, StatsType::None) => return,
        (ReportBackend::Markdown, stats_type) => {
            pretty_print(stats_type, &header, &stats);
        }
        (ReportBackend::Binary, StatsType::Full) => {
            binary_serialize(&header, &stats);
        }
        (ReportBackend::Binary | ReportBackend::Python, StatsType::Short) => {
            // (Lowest priority use case)
            // Just CSV of iter stats.
            // I don't see a real need for a binary format for just iter stats.
            unimplemented!();
        }
        (ReportBackend::Python, StatsType::Full) => {
            // FIXME: refine to reduce need for cleanup

            // dictionary on each line. Debug print gets close but has Type names. just want field names
            println!("{:?}", header);
            println!("{:?}", stats);
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
    // Recorded in header so that we can still compare data as the implementation changes
    elem_sizes: ElemSizes,
}
impl BinHeader {
    pub const CURRENT_VERSION: u32 = 0;
    pub fn new(num_threads: Option<NonZeroU32>, elem_sizes: ElemSizes) -> Self {
        let mut header = Self {
            version: BinHeader::CURRENT_VERSION,
            header_checksum: 0,
            num_threads,
            elem_sizes,
        };
        header.update_checksum();
        header
    }
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
fn pretty_print(stats_type: StatsType, header: &BinHeader, stats: &HistList) {
    println!("# Samples Summary\n");

    if let Some(count) = header.num_threads {
        println!("multi-threaded: {count}")
    } else {
        println!("single-threaded")
    }

    println!("\n<details><summary>State Hashes</summary>\n\n```");
    for (state_hash, count) in stats.state_hash.iter() {
        println!("{state_hash:16x} | {count}")
    }
    println!("```\n</details>\n");

    // TODO: replace this with properties from diff
    println!(
        "| {:^11} | {:^12} | {:^13} | {:^13} | {:^13} |",
        "# Iters", "Time (ms)", "Iters / sec", "Phys Mem MB", "Virt Mem MB"
    );
    println!(
        "|{}:|{}:|{}:|{}:|{}:|",
        "-".repeat(12),
        "-".repeat(13),
        "-".repeat(14),
        "-".repeat(14),
        "-".repeat(14),
    );
    println!(
        "| {:11.0} | {:12.0} | {:13.0} | {:13.0} | {:13.0} |",
        stats.iter_count.mean(),
        stats.total_ms.mean(),
        stats.iter_count.weighted_sum() as f64 / stats.total_ms.weighted_sum() as f64 * 1000.,
        stats.phys_mem_usage.mean() / 1_000_000.,
        stats.virt_mem_usage.mean() / 1_000_000.,
    );

    if stats_type != StatsType::Full {
        return;
    }
    println!("\n");
    println!("# Memory Breakdown\n");

    let num_samples = stats.state_hash.unweighted_sum();
    // Physical memory is more interesting, so we use as the total
    if stats.phys_mem_usage.len() == 0 {
        eprintln!("Process memory usage stats completely absent, omitting");
    } else if stats.phys_mem_usage.unweighted_sum() != num_samples {
        eprintln!("1 or more samples are missing process memory usage data, stats may be skewed");
    }
    let avg_proc_mb_used = stats.phys_mem_usage.mean() / 1_000_000.;
    let avg_proc_mb_reserved = stats.virt_mem_usage.mean() / 1_000_000.;

    let mem_table = [
        (
            "child_map::(K,V)",
            header.elem_sizes.child_map_kv,
            &stats.map_len,
            &stats.map_cap,
        ),
        (
            "Node",
            header.elem_sizes.node,
            &stats.node_len,
            &stats.node_cap,
        ),
        (
            "MoveNode",
            header.elem_sizes.move_node,
            &stats.move_node_len,
            &stats.move_node_cap,
        ),
        (
            "Instruction",
            header.elem_sizes.instruction,
            &stats.instr_list_len,
            &stats.instr_list_cap,
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
        mem_table
            .iter()
            .fold((0, 0), |(uacc, racc), (_, size, used, reserved)| {
                (
                    uacc + used.weighted_sum() * *size as u64,
                    racc + reserved.weighted_sum() * *size as u64,
                )
            });
    let sum_mb_used = sum_mb_used as f64 / 1_000_000. / num_samples as f64;
    let sum_mb_reserved = sum_mb_reserved as f64 / 1_000_000. / num_samples as f64;
    let avg_mb_used = avg_proc_mb_used.max(sum_mb_used);
    let avg_mb_reserved = avg_proc_mb_reserved.max(sum_mb_reserved);
    let pct_total_spare = 100. * (avg_mb_reserved - avg_mb_used) / avg_mb_reserved;

    for (name, size, used, reserved) in mem_table {
        let used = used.weighted_sum() / num_samples;
        let reserved = reserved.weighted_sum() / num_samples;
        assert!(
            used <= reserved,
            "faulty data, capacity shouldn't be less than len"
        );
        let used_mb = (used * size as u64) as f64 / 1_000_000f64;
        let reserved_mb = (reserved * size as u64) as f64 / 1_000_000f64;
        let pct_spare = (1f64 - used as f64 / reserved as f64) * 100f64;
        let pct_total = 100f64 * reserved_mb as f64 / avg_mb_reserved as f64;
        println!(
            "| {name:20} | {size:8} | {used:12} | {used_mb:13.2} | {reserved:12} | {reserved_mb:13.2} | {pct_spare:9.2} | {pct_total:9.2} |"
        );
    }
    println!("||");
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
    println!("||");
    println!(
        "| {:20} | {:8} | {:12} | {:13.2} | {:12} | {:13.2} | {:9.2} | {:9} |",
        "Grand Total", "", "", avg_mb_used, "", avg_mb_reserved, pct_total_spare, 100f64
    );

    // println!("\n");
    // println!("# Time Breakdown\n");

    println!("\n\n# Histograms\n");

    render_hists(&[("", &header, &stats)]);
}

type Report<'a> = (&'a str, &'a BinHeader, &'a HistList);

fn diff(reports: &[Report]) {
    let Some((baseline, others)) = reports.split_first() else {
        return;
    };
    if others.len() != 1 {
        println!("# State hashes\n");
        println!(
            "'{}' (Baseline) has {} states",
            baseline.0,
            baseline.2.state_hash.unweighted_sum()
        );
        for other in others {
            let num_matching_state_hashes = baseline
                .2
                .state_hash
                .iter()
                .filter(|b| other.2.state_hash.iter().any(|o| b.0 == o.0))
                .count();
            println!(
                "'{}': {num_matching_state_hashes} / {} states in common with baseline",
                other.0,
                baseline.2.state_hash.unweighted_sum()
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

    // TODO: replace pretty-print with this
    // types of histograms
    // T0: low-to-high: prefix sum etc
    // T1: subcomponents: show pct total
    // T2: others normalized by iter|sample|thread
    // T3: some may want standard deviation or other

    // TODO: maybe a tag on each property so they can be filtered by diff flags
    fn prop_fn(r: &Report) -> [(&'static str, f64); 23] {
        let bh = &r.1;
        let es = &bh.elem_sizes;
        let stats = &r.2;
        let total_iters = stats.iter_count.weighted_sum();
        let total_bytes_used = es.node as u64 * stats.node_len.weighted_sum()
            + es.move_node as u64 * stats.move_node_len.weighted_sum()
            + es.instruction as u64 * stats.instr_list_len.weighted_sum()
            + es.child_map_kv as u64 * stats.map_len.weighted_sum();
        let total_bytes_reserved = es.node as u64 * stats.node_cap.weighted_sum()
            + es.move_node as u64 * stats.move_node_cap.weighted_sum()
            + es.instruction as u64 * stats.instr_list_cap.weighted_sum()
            + es.child_map_kv as u64 * stats.map_cap.weighted_sum();
        let total_time_ms = stats.total_ms.weighted_sum() as f64;
        let pct_spare_bytes =
            100f64 * (1f64 - total_bytes_used as f64 / total_bytes_reserved as f64);
        let num_samples = stats.state_hash.unweighted_sum();
        let num_samplesf = num_samples as f64;
        let total_itersf = total_iters as f64;
        let num_nodes = stats.node_len.weighted_sum();
        let n_threads = r.1.num_threads.map(NonZeroU32::get).unwrap_or(1);
        // total subtimes across samples, in seconds
        let tot_selection = stats.selection_ms.weighted_sum() as f64;
        let tot_expand = stats.expand_ms.weighted_sum() as f64;
        let tot_rollout = stats.rollout_ms.weighted_sum() as f64;
        let tot_backpropagate = stats.backpropagate_ms.weighted_sum() as f64;
        let tot_unaccounted_time =
            total_time_ms - tot_selection - tot_expand - tot_rollout - tot_backpropagate;
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
            ("avg ChildMap.len / sample", stats.map_len.weighted_sum() as f64 / num_samplesf),
            ("avg ChildMap.cap / sample", stats.map_cap.weighted_sum() as f64 / num_samplesf),
            ("avg MB used / sample", total_bytes_used as f64 / 1_000_000. / num_samplesf),
            ("avg MB reserved / sample", total_bytes_reserved as f64 / 1_000_000. / num_samplesf),
            ("avg B used / iter", total_bytes_used as f64 / total_itersf),
            ("avg B reserved / iter", total_bytes_reserved as f64 / total_itersf),
            ("% spare bytes", pct_spare_bytes),
            ("avg iters / sample", total_itersf / num_samplesf),
            ("avg time (ms) / sample", total_time_ms / num_samplesf),
            ("avg iters / sec", total_itersf * 1000. / total_time_ms),
            ("avg iters / sec / thread", total_itersf * 1000. / total_time_ms / n_threads as f64),
            ("% time in selection", 100. * tot_selection / total_time_ms),
            ("% time in expand", 100. * tot_expand / total_time_ms),
            ("% time in rollout", 100. * tot_rollout / total_time_ms),
            ("% time in backpropagate", 100. * tot_backpropagate / total_time_ms),
            ("% time unaccounted", 100. * tot_unaccounted_time / total_time_ms),
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
    for (hist_idx, (hist_name, display_type)) in HistList::LABELS
        .iter()
        .zip(HistList::DISPLAY_TYPE)
        .enumerate()
    {
        if *display_type == DisplayType::Skip {
            continue;
        }
        // currently only Seq has a good generic display
        // Others are handled ad-hoc
        if *display_type != DisplayType::Seq {
            continue;
        }
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
            .map(|r| r.2.as_array()[hist_idx].stat_iter().peekable())
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
    }
}

fn binary_serialize(header: &BinHeader, stats: &HistList) {
    let mut scratch_buf = Vec::<u8>::with_capacity(4096 * 4);

    scratch_buf.extend_from_slice(unsafe {
        core::slice::from_raw_parts(
            core::ptr::from_ref(header).cast::<u8>(),
            size_of::<BinHeader>(),
        )
    });

    for hist in stats.as_array() {
        hist.serialize(&mut scratch_buf);
    }

    stdout().write_all(&scratch_buf).unwrap();
    stdout().flush().unwrap();
}

// TODO: replace panic with Result, I guess
fn binary_deserialize(data: &[u8]) -> (BinHeader, HistList) {
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

    let mut stats = HistList::new();
    let mut rest = rest;
    for hist in stats.as_array_mut() {
        rest = hist.deserialize(rest).unwrap();
    }
    (header, stats)
}
