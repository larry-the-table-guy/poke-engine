//! Structs that track relevant properties of the MCTS search

use poke_engine::{
    arena::{Arena, Handle},
    mcts, mcts_threaded,
};
use std::collections::BTreeMap;

/// How a histogram should be displayed
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum DisplayType {
    /// Sequential data with many instances, like Vec::len
    Seq,
    /// Average over samples
    Avg, // or Norm?
    /// Sub component of time
    Time,
    /// Don't display this histogram
    Skip,
}

macro_rules! mk_Stats {
    ($($(#[$meta: meta])* $field: ident $label: literal $display_type: ident,)*) => {

        /// Metrics for a run of [perform_mcts]
        #[derive(Debug)]
        #[repr(C)]
        pub struct Stats {
            // SAFETY: this must only contain [Histogram]s
            $(
                $(#[$meta])*
                pub $field: Histogram,
            )*
        }
        impl Stats {
            /// How many histograms
            pub const COUNT: usize = size_of::<Stats>() / size_of::<Histogram>();
            /// For displaying histograms
            pub const LABELS: &[&str; Self::COUNT] = &[$($label,)*];
            /// How each field ought to be displayed
            pub const DISPLAY_TYPE: &[DisplayType; Self::COUNT] = &[$(DisplayType::$display_type,)*];
            /// for Python serialization
            pub const _FIELD_NAMES: &[&str; Self::COUNT] = &[$(stringify!($field),)*];
            pub const fn new() -> Self {
                Self {
                    $($field: Histogram::new(),)*
                }
            }
        }
    };
}

mk_Stats!(
    /// Hash of the input state string
    state_hash "state_hash" Skip,

    /// How many iterations of MCTS were performed
    iter_count "num_iters" Avg,

    /// Process memory usage immediately after mcts before. Zero if absent
    phys_mem_usage "phys_mem_usage" Avg,
    /// Process memory usage immediately after mcts. Zero if absent
    virt_mem_usage "virt_mem_usage" Avg,

    /// Duration of MCTS run in milliseconds
    total_ms "time (ms)" Avg,
    /// Time spent in selection stage
    selection_ms "selection (ms)" Time,
    /// Time spent in expand stage
    expand_ms "expand (ms)" Time,
    /// Time spent in rollout stage
    rollout_ms "rollout (ms)" Time,
    /// Time spent in backpropagate stage
    backpropagate_ms "backpropagate (ms)" Time,
    /// Time spent idle
    idle_ms "backpropagate (ms)" Time,

    /// Final len of ChildMap
    map_len "child_map.len" Avg,
    /// Final capacity of ChildMap
    map_cap "child_map.capacity" Avg,

    /// Vec<Node> in ChildMap::V
    node_len "child_map[k].len" Seq,
    /// Vec<Node> in ChildMap::V
    node_cap "child_map[k].cap" Seq,

    /// Node.s[12]_options
    move_node_len "node.options.len" Seq,
    /// Node.s[12]_options
    move_node_cap "node.options.cap" Seq,

    /// How many times nodes appeared as a key in ChildMap
    ///
    /// In other words, how many of its options were explored
    node_as_key "node.explored_options" Seq,
    /// s1.len() * s2.len()
    options_product "options.cartprod.len" Seq,

    /// Node.instructions_list
    instr_list_len "node.instrs.len" Seq,
    /// Node.instructions_list
    instr_list_cap "node.instrs.cap" Seq,

    /// Depth of any nodes in ChildMap
    node_depth "node.depth" Seq,
    /// Depth of leaf nodes in ChildMap
    leaf_node_depth "leaf_node.depth" Seq,

    // TODO: am i misinterpreting this?

    /// Node.instructions.percentage * 10
    node_weight_pct "node.instrs.pct" Seq,
);

impl Stats {
    // SAFETY: type only contains Histograms and is repr(C)
    pub const fn as_array(&self) -> &[Histogram; Self::COUNT] {
        unsafe { core::mem::transmute(self) }
    }
    pub fn as_array_mut(&mut self) -> &mut [Histogram] {
        unsafe {
            core::mem::transmute::<&mut Self, &mut [Histogram; Self::COUNT]>(self).as_mut_slice()
        }
    }

    pub fn analyze_tree<'a>(
        &mut self,
        arena: &Arena,
        root: Handle<'a, mcts::Node<'a>>,
        child_map: &mcts::ChildMap<'a>,
    ) {
        self.map_len.inc(child_map.len() as u64);
        self.map_cap.inc(child_map.capacity() as u64);
        let mut tmp_node_num_children_hist = Histogram::new();
        for (k, v) in child_map.iter() {
            let v = v.resolve(arena);
            tmp_node_num_children_hist.inc(k.0.resolve(arena) as *const _ as u64);
            self.node_len.inc(v.len() as u64);
            self.node_cap.inc(v.len() as u64);
            for node in v {
                if let Some(options) = node.options.get() {
                    let options = options.resolve(arena);
                    self.move_node_len.inc(options.s1().len() as u64);
                    self.move_node_cap.inc(options.s1().len() as u64);
                    self.move_node_len.inc(options.s2().len() as u64);
                    self.move_node_cap.inc(options.s2().len() as u64);
                    self.options_product
                        .inc(options.s1().len() as u64 * options.s2().len() as u64);
                } else {
                    self.move_node_len.add(0, 2);
                    self.move_node_cap.add(0, 2);
                    self.options_product.inc(0);
                }
                let ins = node.instruction_list.resolve(arena);
                self.instr_list_len.inc(ins.len() as u64);
                self.instr_list_cap.inc(ins.len() as u64);

                self.node_weight_pct.inc((node.percentage).floor() as u64);
            }
        }
        for count in tmp_node_num_children_hist.0.values() {
            self.node_as_key.inc(*count as u64);
        }
        drop(tmp_node_num_children_hist);

        // This can be written with an explicit stack, but the depth is just in the 10s.
        fn visit_children<'a>(
            node: Handle<'a, mcts::Node<'a>>,
            arena: &Arena,
            child_map: &mcts::ChildMap<'a>,
            node_depth_hist: &mut Histogram,
            leaf_node_depth_hist: &mut Histogram,
            depth: usize,
        ) {
            node_depth_hist.inc(depth as u64);
            let Some(options) = node.resolve(arena).options.get() else {
                leaf_node_depth_hist.inc(depth as u64);
                return;
            };
            let options = options.resolve(arena);
            if options.s1().is_empty() || options.s2().is_empty() {
                leaf_node_depth_hist.inc(depth as u64);
                return;
            }
            for s1 in 0..options.s1().len() {
                for s2 in 0..options.s2().len() {
                    if let Some(entry) = child_map.get(&(node, s1 as u8, s2 as u8)) {
                        for child in entry.iter() {
                            visit_children(
                                child,
                                arena,
                                child_map,
                                node_depth_hist,
                                leaf_node_depth_hist,
                                depth + 1,
                            );
                        }
                    }
                }
            }
        }
        visit_children(
            root,
            arena,
            child_map,
            &mut self.node_depth,
            &mut self.leaf_node_depth,
            0,
        );
    }

    pub fn analyze_threaded_tree<'a>(
        &mut self,
        arena: &Arena<'a>,
        root: Handle<'a, mcts_threaded::Node<'a>>,
        child_map: &mcts_threaded::ChildMap<'a>,
    ) {
        self.map_len.inc(child_map.len() as u64);
        self.map_cap.inc(child_map.capacity() as u64);
        let mut tmp_node_num_children_hist = Histogram::new();
        for r in child_map.iter() {
            let (k, v) = r.pair();
            tmp_node_num_children_hist.inc(k.0.resolve(arena) as *const _ as u64);
            self.node_len.inc(v.len() as u64);
            self.node_cap.inc(v.len() as u64);
            for node in v.resolve(arena).iter() {
                let node = node;
                if let Some(options) = node.options.get() {
                    let options = options.resolve(arena);
                    self.move_node_len.inc(options.s1().len() as u64);
                    self.move_node_cap.inc(options.s1().len() as u64);
                    self.move_node_len.inc(options.s2().len() as u64);
                    self.move_node_cap.inc(options.s2().len() as u64);
                    self.options_product
                        .inc(options.s1().len() as u64 * options.s2().len() as u64);
                } else {
                    self.move_node_len.add(0, 2);
                    self.move_node_cap.add(0, 2);
                    self.options_product.inc(0);
                }
                let ins = &node.instruction_list;
                self.instr_list_len.inc(ins.len() as u64);
                self.instr_list_cap.inc(ins.len() as u64);

                self.node_weight_pct.inc((node.percentage).floor() as u64);
            }
        }
        for count in tmp_node_num_children_hist.0.values() {
            self.node_as_key.inc(*count as u64);
        }
        drop(tmp_node_num_children_hist);

        // This can be written with an explicit stack, but the depth is just in the 10s.
        fn visit_children<'a>(
            node: Handle<'a, mcts_threaded::Node<'a>>,
            child_map: &mcts_threaded::ChildMap<'a>,
            node_depth_hist: &mut Histogram,
            leaf_node_depth_hist: &mut Histogram,
            depth: usize,
            arena: &Arena<'a>,
        ) {
            node_depth_hist.inc(depth as u64);
            let Some(options) = node.resolve(arena).options.get() else {
                leaf_node_depth_hist.inc(depth as u64);
                return;
            };
            let options = options.resolve(arena);
            if options.s1().is_empty() || options.s2().is_empty() {
                leaf_node_depth_hist.inc(depth as u64);
                return;
            }
            for s1 in 0..options.s1().len() {
                for s2 in 0..options.s2().len() {
                    if let Some(entry) = child_map.get(&(node, s1 as u8, s2 as u8)) {
                        for child in entry.iter() {
                            visit_children(
                                child,
                                child_map,
                                node_depth_hist,
                                leaf_node_depth_hist,
                                depth + 1,
                                arena,
                            );
                        }
                    }
                }
            }
        }
        visit_children(
            root,
            child_map,
            &mut self.node_depth,
            &mut self.leaf_node_depth,
            0,
            arena,
        );
    }
}

#[derive(Default, Debug)]
pub struct Histogram(BTreeMap<u64, usize>);

impl Histogram {
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }
    pub fn add(&mut self, k: u64, n: usize) {
        *self.0.entry(k).or_default() += n;
    }
    pub fn inc(&mut self, k: u64) {
        self.add(k, 1);
    }
    /// How many columns
    pub fn len(&self) -> usize {
        self.0.len()
    }
    /// Sum(v for v in self.values)
    pub fn unweighted_sum(&self) -> u64 {
        self.0.values().map(|v| *v as u64).sum()
    }
    /// Sum(k*v for k,v in self)
    pub fn weighted_sum(&self) -> u64 {
        self.0.iter().map(|(k, v)| k * *v as u64).sum()
    }

    pub fn mean(&self) -> f64 {
        self.weighted_sum() as f64 / self.unweighted_sum() as f64
    }

    // TODO: compact encoding if we end up storing a lot more data
    #[inline(never)]
    pub fn deserialize<'a>(&mut self, buf: &'a [u8]) -> Result<&'a [u8], ()> {
        const W: usize = size_of::<u64>();
        let (len, rest) = buf.split_first_chunk::<W>().ok_or(())?;
        let len = u64::from_ne_bytes(*len) as usize;
        let (data, rest) = rest.split_at_checked(2 * W * len).ok_or(())?;
        for [k, v] in data.as_chunks::<W>().0.as_chunks::<2>().0 {
            self.add(u64::from_ne_bytes(*k), u64::from_ne_bytes(*v) as usize);
        }
        Ok(rest)
    }
    pub fn serialize(&self, out: &mut Vec<u8>) {
        out.reserve(1 + self.len() * 2 * size_of::<u64>());
        out.extend_from_slice(&(self.len() as u64).to_ne_bytes());
        out.extend(
            self.0
                .iter()
                .flat_map(|(k, v)| [(*k as u64).to_ne_bytes(), (*v as u64).to_ne_bytes()].concat()),
        );
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, usize)> + use<'_> {
        self.0.iter().map(|(k, v)| (*k, *v))
    }
    /// (Key, Count, % Total, % Prefix Sum, % Suffix Sum)
    pub fn stat_iter(&self) -> impl Iterator<Item = (u64, usize, f64, f64, f64)> + use<'_> {
        let total_count = self.iter().map(|(_, v)| v).sum::<usize>() as f64;
        let inv_total_count = 100f64 / total_count;
        let mut prefix_sum = 0f64;
        self.iter().map(move |(k, count)| {
            let fcount = count as f64;
            let ssum_pct = 100. * fcount / (total_count - prefix_sum);
            prefix_sum += fcount;
            let pct_total = fcount * inv_total_count;
            let psum_pct = prefix_sum * inv_total_count;
            (k, count, pct_total, psum_pct, ssum_pct)
        })
    }
}
