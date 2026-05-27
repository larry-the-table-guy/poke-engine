//! Structs that track relevant properties of the MCTS search

use poke_engine::mcts::Timers;
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

    pub fn analyze_time(&mut self, timers: Timers) {
        self.selection_ms.inc(timers.selection / 1_000_000);
        self.expand_ms.inc(timers.expand / 1_000_000);
        self.rollout_ms.inc(timers.rollout / 1_000_000);
        self.backpropagate_ms.inc(timers.backpropagate / 1_000_000);
        self.idle_ms.inc(timers.idle / 1_000_000);
    }

    pub fn analyze_tree(&mut self, child_map: &poke_engine::mcts::ChildMap) {
        self.map_len.inc(child_map.len() as u64);
        self.map_cap.inc(child_map.capacity() as u64);
        let mut tmp_node_num_children_hist = Histogram::new();
        for (k, v) in child_map.iter() {
            let parent =
                unsafe { &*std::ptr::with_exposed_provenance::<poke_engine::mcts::Node>(k.0) };
            tmp_node_num_children_hist.inc(k.0 as u64);
            self.node_len.inc(v.len() as u64);
            self.node_cap.inc(v.len() as u64);
            for node in v {
                if let Some(options) = node.options.as_ref() {
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
                let ins = &node.instructions.instruction_list;
                self.instr_list_len.inc(ins.len() as u64);
                self.instr_list_cap.inc(ins.capacity() as u64);

                let mut depth = 0;
                let mut nd = std::ptr::from_ref(parent);
                assert_eq!(node.root, node.parent.is_null());
                while !nd.is_null() {
                    nd = unsafe { (&*nd).parent };
                    depth += 1;
                }
                self.node_depth.inc(depth);
                if node.options.is_none() {
                    self.leaf_node_depth.inc(depth);
                }

                self.node_weight_pct
                    .inc((node.instructions.percentage).floor() as u64);
            }
        }
        for count in tmp_node_num_children_hist.0.values() {
            self.node_as_key.inc(*count as u64);
        }
    }

    pub fn analyze_threaded_tree(&mut self, child_map: &poke_engine::mcts_threaded::ChildMap) {
        self.map_len.inc(child_map.len() as u64);
        self.map_cap.inc(child_map.capacity() as u64);
        let mut tmp_node_num_children_hist = Histogram::new();
        for r in child_map.iter() {
            let (k, v) = r.pair();
            let v = &v.nodes;
            tmp_node_num_children_hist.inc(k.0 as u64);
            self.node_len.inc(v.len() as u64);
            self.node_cap.inc(v.len() as u64);
            for node in v.iter() {
                if let Some(options) = node.options.get() {
                    self.move_node_len.inc(options.s1().len() as u64);
                    self.move_node_cap.inc(options.s1().len() as u64);
                    self.move_node_len.inc(options.s2().len() as u64);
                    self.move_node_cap.inc(options.s2().len() as u64);
                    self.options_product
                        .inc(options.s1().len() as u64 * options.s2().len() as u64);
                }
                let ins = &node.instructions.instruction_list;
                self.instr_list_len.inc(ins.len() as u64);
                self.instr_list_cap.inc(ins.capacity() as u64);

                self.node_depth.inc(node.depth as u64);
                if node.options.get().is_none_or(|s| s.s1().is_empty()) {
                    self.leaf_node_depth.inc(node.depth as u64);
                }

                self.node_weight_pct
                    .inc((node.instructions.percentage).floor() as u64);
            }
        }
        for count in tmp_node_num_children_hist.0.values() {
            self.node_as_key.inc(*count as u64);
        }
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
