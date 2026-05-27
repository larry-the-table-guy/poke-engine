use core::hash::BuildHasher;
use core::num::NonZeroU32;
use core::time::Duration;
use std::io::{stdout, IsTerminal, Read, Write};

use poke_engine::instruction;
use poke_engine::state::State;
use poke_engine::{mcts, mcts_threaded};

use foldhash::quality as fhash;

use profiling::{DisplayType, Stats};

mod profiling;

fn main() {
    let args = std::env::args()
        .skip(1)
        .filter(|s| s != "--bench") // ignore, passed by cargo
        .collect::<Vec<String>>();

    let (command, args) = args.split_first().expect("need at least one arg for mode");

    match command.as_str() {
        "bench" => {
            // parse --key=value flags
            let mut skip_stats = false;
            let mut max_time = Duration::from_secs(5);
            let mut num_threads: Option<NonZeroU32> = None; // default to single threaded
            for arg in args {
                if arg == "--skip-stats" {
                    skip_stats = true;
                } else if let Some(seconds) = arg.strip_prefix("--time=") {
                    max_time = Duration::from_secs(seconds.parse().unwrap());
                } else if let Some(count) = arg.strip_prefix("--threads=") {
                    let count = count.parse::<u32>().expect("valid u32 thread count");
                    num_threads = NonZeroU32::new(count);
                } else if arg.starts_with("--") {
                    panic!("unrecognized argument '{}'", arg)
                } else {
                    // ignore, handled by each separate command
                }
            }
            if !skip_stats && std::io::stdout().is_terminal() {
                panic!(
                    "Hey! 'bench' produces binary output, but stdout is a terminal. Redirect stdout."
                );
            }
            bench_mcts(num_threads, max_time, skip_stats);
        }
        "diff" => {
            let files = args;
            let mut buf = Vec::<u8>::new();
            let mut title = None;
            let mut short = false;
            let reports = files
                .iter()
                .filter(|s| {
                    if let Some(t) = s.strip_prefix("--title=") {
                        title = Some(t);
                        false
                    } else if *s == "--short" {
                        short = true;
                        false
                    } else {
                        true
                    }
                })
                .map(|s| {
                    buf.clear();
                    let p = std::path::Path::new(&s);
                    let mut file = std::fs::OpenOptions::new().read(true).open(p).unwrap();
                    file.read_to_end(&mut buf).unwrap();
                    let (a, b) = binary_deserialize(buf.as_slice());
                    // was originally a str, and we managed to open it as a file so it's not ".."
                    let name = p.file_name().unwrap().to_str().unwrap().to_owned();
                    (name, a, b)
                })
                .collect::<Vec<_>>();
            let reports = reports
                .iter()
                .map(|r| (r.0.as_str(), &r.1, &r.2))
                .collect::<Vec<_>>();
            diff(reports.as_slice(), short, title);
        }
        "print-hashes" => {
            let mut input = Vec::with_capacity(4096 * 2);
            std::io::stdin().read_to_end(&mut input).unwrap();
            let (_, b) = binary_deserialize(&input);
            for (hash, count) in b.state_hash.iter() {
                println!("{hash:16x} x {count}");
            }
        }
        "print-python" => {
            unimplemented!();
        }
        "merge" => {
            // take file paths from args, compare version numbers, combine hists
            todo!("");
        }
        s if s.starts_with("--") => panic!("missing mode argument"),
        s => panic!("unrecognized mode '{}'", s),
    }
}

fn bench_mcts(num_threads: Option<NonZeroU32>, max_time: Duration, skip_stats: bool) {
    let mut stats = Stats::new();
    let mut at_least_one = false;

    for line in std::io::stdin()
        .lines()
        .map(|r| r.unwrap())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
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
            let time_ms = start.elapsed().as_millis() as u64;
            if !skip_stats {
                proc_mem_usage = memory_stats::memory_stats();
                stats.analyze_threaded_tree(&childmap);
            }
            (r.iteration_count, time_ms, timers)
        } else {
            let (r, _root, timers, childmap) =
                mcts::perform_mcts_inner(&mut state, s1_options, s2_options, max_time);
            let time_ms = start.elapsed().as_millis() as u64;
            if !skip_stats {
                proc_mem_usage = memory_stats::memory_stats();
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

    if !at_least_one || skip_stats {
        return;
    }
    let elem_sizes = if num_threads.is_none() {
        ElemSizes::CURRENT
    } else {
        ElemSizes::CURRENT_THREADED
    };
    let header = BinHeader::new(num_threads, elem_sizes.clone());

    binary_serialize(&header, &stats);
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
    /// Which generation was used.
    gen: u32,
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
    pub const CURRENT_VERSION: u32 = 1;
    /// First bytes in binary report, helpful if someone opens in text editor
    ///
    /// Part of the serialization format, doubles as magic bytes.
    pub const PREFIX_STRING: &str = "Binary benchmark report for poke-engine\n";
    pub fn new(num_threads: Option<NonZeroU32>, elem_sizes: ElemSizes) -> Self {
        let gen = cfg_select! {
            feature = "gen9" => 9,
            feature = "gen8" => 8,
            feature = "gen7" => 7,
            feature = "gen6" => 6,
            feature = "gen5" => 5,
            feature = "gen4" => 4,
            feature = "gen3" => 3,
            feature = "gen2" => 2,
            feature = "gen1" => 1,
        };
        let mut header = Self {
            version: BinHeader::CURRENT_VERSION,
            gen,
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
        // ArcInner -> 2 usizes
        node: (size_of::<(usize, usize)>() + size_of::<mcts_threaded::Node>()) as u32,
        move_node: size_of::<mcts_threaded::MoveNode>() as u32,
        instruction: size_of::<instruction::Instruction>() as u32,
    };
}

type Report<'a> = (&'a str, &'a BinHeader, &'a Stats);

fn diff(reports: &[Report], short: bool, title: Option<&str>) {
    let Some((baseline, others)) = reports.split_first() else {
        return;
    };
    if others.len() != 1 {
        println!("### State hashes\n");
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
        println!("\n");
    }

    println!(
        "<details><summary><h1>{}</h1></summary>\n",
        title.unwrap_or("Summary")
    );

    print!("| {:^29} | {:^14} |", "Property", "Baseline",);
    for other in others {
        print!(
            " {:^14.14} | {:^14} | {:^8} |",
            other.0, "Change", "Relative"
        );
    }
    println!();
    print!("|:{}|{}:|", "-".repeat(30), "-".repeat(15));
    for _ in others {
        print!(
            "{}:|{}:|{}:|",
            "-".repeat(15),
            "-".repeat(15),
            "-".repeat(9)
        );
    }
    println!();

    // TODO: add some line breaks, hard to read as it gets longer
    // a few categories of histograms, potential for code reuse

    // TODO: maybe a tag on each property so they can be filtered by diff flags
    fn prop_fn(r: &Report) -> [(&'static str, f64); 39] {
        const MEGA: f64 = 1_000_000.;
        let bh = &r.1;
        let es = &bh.elem_sizes;
        let stats = &r.2;
        let total_iters = stats.iter_count.weighted_sum();
        let n_threads = r.1.num_threads.map(NonZeroU32::get).unwrap_or(1);

        let tot_used_nodes = stats.node_len.weighted_sum();
        let tot_used_movenodes = stats.move_node_len.weighted_sum();
        let tot_used_instrs = stats.instr_list_len.weighted_sum();
        let tot_used_map = stats.map_len.weighted_sum();

        let tot_reserved_nodes = stats.node_cap.weighted_sum();
        let tot_reserved_movenodes = stats.move_node_cap.weighted_sum();
        let tot_reserved_instrs = stats.instr_list_cap.weighted_sum();
        let tot_reserved_map = stats.map_cap.weighted_sum();
        let tot_res_nodes_b = es.node as u64 * tot_reserved_nodes;
        let tot_res_movenodes_b = es.move_node as u64 * tot_reserved_movenodes;
        let tot_res_instrs_b = es.instruction as u64 * tot_reserved_instrs;
        let tot_res_map_b = es.child_map_kv as u64 * tot_reserved_map;
        let total_bytes_reserved =
            tot_res_nodes_b + tot_res_movenodes_b + tot_res_instrs_b + tot_res_map_b;

        let total_time_ms = stats.total_ms.weighted_sum() as f64;
        let total_thread_time_ms = total_time_ms * n_threads as f64;
        let proc_phys = stats.phys_mem_usage.weighted_sum() as f64;
        let pct_unaccounted = 100. * (proc_phys - total_bytes_reserved as f64) / proc_phys;
        let num_samples = stats.state_hash.unweighted_sum();
        let num_samplesf = num_samples as f64;
        let total_itersf = total_iters as f64;
        let num_nodes = stats.node_len.weighted_sum();
        // total subtimes across samples, in seconds
        let tot_selection = stats.selection_ms.weighted_sum() as f64;
        let tot_expand = stats.expand_ms.weighted_sum() as f64;
        let tot_rollout = stats.rollout_ms.weighted_sum() as f64;
        let tot_backpropagate = stats.backpropagate_ms.weighted_sum() as f64;
        let tot_idle = stats.idle_ms.weighted_sum() as f64;
        let tot_unaccounted_time = total_time_ms
            - (tot_selection + tot_expand + tot_rollout + tot_backpropagate + tot_idle)
                / n_threads as f64;
        #[rustfmt::skip]
        let a = [
            ("Gen", bh.gen as f64),
            ("Threads", bh.num_threads.map(NonZeroU32::get).unwrap_or_default() as f64),

            ("size(`ChildMap::KV`)", es.child_map_kv as f64),
            ("size(`Node`)", es.node as f64),
            ("size(`MoveNode`)", es.move_node as f64),
            ("size(`Instruction`)", es.instruction as f64),

            ("used `Node`s / sample", tot_used_nodes as f64 / num_samplesf),
            ("reserved `Node`s / sample", tot_reserved_nodes as f64 / num_samplesf),
            ("% used `Node`s", 100. * tot_used_nodes as f64 / tot_reserved_nodes as f64),
            ("`Node` MB / sample", tot_res_nodes_b as f64 / MEGA / num_samplesf),

            ("used `MoveNode`s / sample", tot_used_movenodes as f64 / num_samplesf),
            ("reserved `MoveNode`s / sample", tot_reserved_movenodes as f64 / num_samplesf),
            ("% used `MoveNode`s", 100. * tot_used_movenodes as f64 / tot_reserved_movenodes as f64),
            ("`MoveNode` MB / sample", tot_res_movenodes_b as f64 / MEGA / num_samplesf),

            ("used `Instr`s / sample", tot_used_instrs as f64 / num_samplesf),
            ("reserved `Instr`s / sample", tot_reserved_instrs as f64 / num_samplesf),
            ("% used `Instruction`s", 100. * tot_used_instrs as f64 / tot_reserved_instrs as f64),
            ("`Instruction` MB / sample", tot_res_instrs_b as f64 / MEGA / num_samplesf),

            ("`ChildMap.len` / sample", tot_used_map as f64 / num_samplesf),
            ("`ChildMap.cap` / sample", tot_reserved_map as f64 / num_samplesf),
            ("% used `ChildMap`", 100. * tot_used_map as f64 / tot_reserved_map as f64),
            ("`ChildMap` MB / sample", tot_res_map_b as f64 / MEGA / num_samplesf),

            ("MB / sample", total_bytes_reserved as f64 / MEGA / num_samplesf),
            ("B / iter", total_bytes_reserved as f64 / total_itersf),
            ("% unaccounted mem", pct_unaccounted),
            ("proc phys MB / sample", proc_phys / MEGA / num_samplesf),
            ("proc virt MB / sample", stats.virt_mem_usage.mean() / MEGA),

            ("`Node`s / iter", num_nodes as f64 / total_itersf),
            ("% explored options", 100. * stats.node_as_key.weighted_sum() as f64 / stats.options_product.weighted_sum() as f64),
            ("iters / sample", total_itersf / num_samplesf),
            ("iters / sec", total_itersf * 1000. / total_time_ms),
            ("iters / sec / thread", total_itersf * 1000. / total_thread_time_ms),

            ("% time in `selection`", 100. * tot_selection / total_thread_time_ms),
            ("% time in `expand`", 100. * tot_expand / total_thread_time_ms),
            ("% time in `rollout`", 100. * tot_rollout / total_thread_time_ms),
            ("% time in `backpropagate`", 100. * tot_backpropagate / total_thread_time_ms),
            ("% time idle", 100. * tot_idle / total_thread_time_ms),
            ("% time unaccounted", 100. * tot_unaccounted_time / total_thread_time_ms),
            ("time (ms) / sample", total_time_ms / num_samplesf),
        ];
        a
    }
    let baseline_props = prop_fn(baseline);
    let other_props = others
        .iter()
        .map(|v| prop_fn(v).map(|t| t.1))
        .collect::<Vec<_>>();
    for (prop_idx, (name, baseline_v)) in baseline_props.iter().enumerate() {
        print!("| {:29} | {:14.2} |", name, baseline_v,);
        for o_v in other_props.iter() {
            let o_v = o_v[prop_idx];
            let change = o_v - baseline_v;
            let relative = o_v / baseline_v;
            // TODO: maybe an option to leave change column blank when identical
            print!(" {:14.2} | {:+14.2} | {:8.3} |", o_v, change, relative);
        }
        println!();
    }
    println!("\n</details>\n");

    if short {
        return;
    }

    println!("# Histograms\n");

    for (hist_idx, (hist_name, display_type)) in
        Stats::LABELS.iter().zip(Stats::DISPLAY_TYPE).enumerate()
    {
        if *display_type == DisplayType::Skip {
            continue;
        }
        // currently only Seq has a good generic display
        // Others are handled ad-hoc
        if *display_type != DisplayType::Seq {
            continue;
        }
        println!("<details><summary><h2><code>{hist_name}</code></h2></summary>\n");

        print!("|    Key    |");
        for idx in 0..reports.len() {
            print!(
                " {:^8} | % Total | %PSum/T | %V/SSum |",
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
        println!("\n</details>\n");
    }
}

fn binary_serialize(header: &BinHeader, stats: &Stats) {
    let mut scratch_buf = Vec::<u8>::with_capacity(4096 * 4);
    scratch_buf.extend_from_slice(BinHeader::PREFIX_STRING.as_bytes());

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

// TODO: replace panics with Result, I guess
fn binary_deserialize(data: &[u8]) -> (BinHeader, Stats) {
    let rest = data
        .strip_prefix(BinHeader::PREFIX_STRING.as_bytes())
        .expect("Input lacks report prefix");
    let (header, rest) = rest.split_at(size_of::<BinHeader>());
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

    let mut stats = Stats::new();
    let mut rest = rest;
    for hist in stats.as_array_mut() {
        rest = hist.deserialize(rest).unwrap();
    }
    (header, stats)
}
