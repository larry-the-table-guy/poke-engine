//! Structs that track relevant properties of the MCTS search

use poke_engine::mcts::Timers;
use std::collections::BTreeMap;

/// Basic stats about a single sample of MCTS
#[repr(C)]
#[derive(Clone, Debug)]
pub struct SampleStats {
    /// Hash of the state string this sample used
    pub state_hash: u64,
    /// Process memory usage immediately after mcts before. Zero if absent
    pub phys_mem_usage: u64,
    /// Process memory usage immediately after mcts. Zero if absent
    pub virt_mem_usage: u64,
    /// How many iterations of MCTS were performed
    pub iter_count: u64,
    /// Duration of MCTS in seconds
    pub seconds: f64,
    /// Duration of subroutines in MCTS in nanoseconds
    pub sub_timers: Timers,
}

/// Stats on the ChildMap for a run of [perform_mcts]
#[derive(Debug)]
pub struct TreeStats {
    /// Final len of ChildMap, cumulative over samples
    pub map_len: usize,
    /// Final capacity of ChildMap, cumulative over samples
    pub map_cap: usize,
    /// Histograms
    pub hists: HistList,
}

impl TreeStats {
    pub const fn new() -> Self {
        TreeStats {
            map_len: 0,
            map_cap: 0,
            hists: HistList::new(),
        }
    }
    pub fn analyze(&mut self, child_map: &poke_engine::mcts::ChildMap) {
        self.map_len += child_map.len();
        self.map_cap += child_map.capacity();
        let mut tmp_node_num_children_hist = Histogram::new();
        for (k, v) in child_map.iter() {
            let parent =
                unsafe { &*std::ptr::with_exposed_provenance::<poke_engine::mcts::Node>(k.0) };
            tmp_node_num_children_hist.inc(k.0);
            self.hists.node_len.inc(v.len());
            self.hists.node_cap.inc(v.capacity());
            for node in v {
                let s1 = node.s1_options.as_ref().unwrap_or(const { &Vec::new() });
                self.hists.move_node_len.inc(s1.len());
                self.hists.move_node_cap.inc(s1.capacity());
                let s2 = node.s2_options.as_ref().unwrap_or(const { &Vec::new() });
                self.hists.move_node_len.inc(s2.len());
                self.hists.move_node_cap.inc(s2.capacity());
                self.hists.options_product.inc(s1.len() * s2.len());
                let ins = &node.instructions.instruction_list;
                self.hists.instr_list_len.inc(ins.len());
                self.hists.instr_list_cap.inc(ins.capacity());

                let mut depth = 0;
                let mut nd = std::ptr::from_ref(parent);
                assert_eq!(node.root, node.parent.is_null());
                while !nd.is_null() {
                    nd = unsafe { (&*nd).parent };
                    depth += 1;
                }
                self.hists.node_depth.inc(depth);
                if node.s1_options.is_none() {
                    assert!(node.s2_options.is_none());
                    self.hists.leaf_node_depth.inc(depth);
                }

                self.hists
                    .node_weight_pct
                    .inc((node.instructions.percentage).floor() as usize);
            }
        }
        for count in tmp_node_num_children_hist.0.values() {
            self.hists.node_as_key.inc(*count);
        }
    }

    pub fn analyze_threaded(&mut self, child_map: &poke_engine::mcts_threaded::ChildMap) {
        self.map_len += child_map.len();
        self.map_cap += child_map.capacity();
        let mut tmp_node_num_children_hist = Histogram::new();
        for r in child_map.iter() {
            let (k, v) = r.pair();
            let v = &v.nodes;
            tmp_node_num_children_hist.inc(k.0);
            self.hists.node_len.inc(v.len());
            self.hists.node_cap.inc(v.capacity());
            for node in v {
                if let Some(options) = node.options.get() {
                    self.hists.move_node_len.inc(options.s1.len());
                    self.hists.move_node_cap.inc(options.s1.capacity());
                    self.hists.move_node_len.inc(options.s2.len());
                    self.hists.move_node_cap.inc(options.s2.capacity());
                    self.hists
                        .options_product
                        .inc(options.s1.len() * options.s2.len());
                }
                let ins = &node.instructions.instruction_list;
                self.hists.instr_list_len.inc(ins.len());
                self.hists.instr_list_cap.inc(ins.capacity());

                self.hists.node_depth.inc(node.depth as usize);
                if node.options.get().is_none_or(|s| s.s1.is_empty()) {
                    self.hists.leaf_node_depth.inc(node.depth as usize);
                }

                self.hists
                    .node_weight_pct
                    .inc((node.instructions.percentage).floor() as usize);
            }
        }
        for count in tmp_node_num_children_hist.0.values() {
            self.hists.node_as_key.inc(*count);
        }
    }
}

macro_rules! mk_HistList {
    ($($(#[$meta: meta])* $field: ident $label: literal,)*) => {

        // SAFETY: this must only contain [Histogram]s
        #[derive(Debug)]
        #[repr(C)]
        pub struct HistList {
            $(
                $(#[$meta])*
                pub $field: Histogram,
            )*
        }
        impl HistList {
            /// How many histograms
            pub const COUNT: usize = size_of::<HistList>() / size_of::<Histogram>();
            /// For displaying histograms
            pub const LABELS: &[&str; Self::COUNT] = &[$($label,)*];
            /// for Python serialization
            pub const _FIELD_NAMES: &[&str; Self::COUNT] = &[$(stringify!($field),)*];
        }
    };
}

mk_HistList!(
    /// How many times nodes appeared as a key in ChildMap
    ///
    /// In other words, how many of its options were explored
    node_as_key "node.explored_options",

    /// Vec<Node> in ChildMap::V
    node_len "child_map[k].len",
    /// Vec<Node> in ChildMap::V
    node_cap "child_map[k].cap",

    /// Node.s[12]_options
    move_node_len "node.options.len",
    /// Node.s[12]_options
    move_node_cap "node.options.cap",
    /// s1.len() * s2.len()
    options_product "options.cartprod.len",

    /// Node.instructions_list
    instr_list_len "node.instrs.len",
    /// Node.instructions_list
    instr_list_cap "node.instrs.cap",

    /// Depth of any nodes in ChildMap
    node_depth "node.depth",
    /// Depth of leaf nodes in ChildMap
    leaf_node_depth "leaf_node.depth",

    // TODO: am i misinterpreting this?

    /// Node.instructions.percentage * 10
    node_weight_pct "node.instrs.pct",
);

impl HistList {
    pub const fn new() -> Self {
        Self {
            node_as_key: Histogram::new(),
            node_len: Histogram::new(),
            node_cap: Histogram::new(),
            move_node_len: Histogram::new(),
            move_node_cap: Histogram::new(),
            options_product: Histogram::new(),
            instr_list_len: Histogram::new(),
            instr_list_cap: Histogram::new(),
            node_depth: Histogram::new(),
            leaf_node_depth: Histogram::new(),
            node_weight_pct: Histogram::new(),
        }
    }
    // SAFETY: type only contains Histograms and is repr(C)
    pub const fn as_array(&self) -> &[Histogram; Self::COUNT] {
        unsafe { core::mem::transmute(self) }
    }
    pub const fn as_array_mut(&mut self) -> &mut [Histogram; Self::COUNT] {
        unsafe { core::mem::transmute(self) }
    }
}

#[derive(Default, Debug)]
pub struct Histogram(BTreeMap<usize, usize>);

impl Histogram {
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }
    pub fn add(&mut self, k: usize, n: usize) {
        *self.0.entry(k).or_default() += n;
    }
    pub fn inc(&mut self, k: usize) {
        self.add(k, 1);
    }
    /// How many columns
    pub fn len(&self) -> usize {
        self.0.len()
    }
    /// Sum(k*v for k,v in self)
    pub fn weighted_sum(&self) -> usize {
        self.0.iter().map(|(k, v)| k * v).sum()
    }

    // TODO: compact encoding if we end up storing a lot more data
    pub fn deserialize<'a>(&mut self, buf: &'a [u8]) -> Result<&'a [u8], ()> {
        const W: usize = size_of::<u64>();
        let (len, rest) = buf.split_first_chunk::<W>().ok_or(())?;
        let len = u64::from_le_bytes(*len) as usize;
        let (data, rest) = rest.split_at_checked(2 * W * len).ok_or(())?;
        for [k, v] in data.as_chunks::<W>().0.as_chunks::<2>().0 {
            self.add(
                u64::from_le_bytes(*k) as usize,
                u64::from_le_bytes(*v) as usize,
            );
        }
        Ok(rest)
    }
    pub fn serialize(&self, out: &mut Vec<u8>) {
        out.reserve(1 + self.len() * 2 * size_of::<u64>());
        out.extend_from_slice(&(self.len() as u64).to_le_bytes());
        out.extend(
            self.0
                .iter()
                .flat_map(|(k, v)| [(*k as u64).to_le_bytes(), (*v as u64).to_le_bytes()].concat()),
        );
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, usize)> + use<'_> {
        self.0.iter().map(|(k, v)| (*k, *v))
    }
    /// (Key, Count, % Total, % Prefix Sum, % Suffix Sum)
    pub fn stat_iter(&self) -> impl Iterator<Item = (usize, usize, f64, f64, f64)> + use<'_> {
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
