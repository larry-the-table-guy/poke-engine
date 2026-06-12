use crate::engine::evaluate::evaluate;
use crate::engine::generate_instructions::generate_instructions_from_move_pair;
use crate::engine::state::MoveChoice;
use crate::instruction::Instruction;
use crate::mcts::{MctsResult, MctsSideResult};
use crate::perf::arena::{Arena, ArenaPool, Handle, SliceHandle};
use crate::state::State;
use dashmap::DashMap;
use rand::prelude::*;
use rand::{rng, rngs::SmallRng as Rng, Rng as _};
use std::sync::atomic::{AtomicI8, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

const MCTS_DEADLINE_CHECK_INTERVAL: u32 = 1_000;
const MCTS_MAX_ITERATIONS_PER_TREE: u32 = 10_000_000;
const MCTS_DAMAGE_BRANCH_DEPTH: usize = 2;
const SCORE_SCALE: f32 = 400.0;
const VIRTUAL_LOSS_VISITS: u32 = 3;

type NodeHandle<'a> = Handle<'a, Node<'a>>;
pub type SharedNodeOptions<'a> = crate::perf::NodeOptions<'a, MoveNode>;
pub type NodeOptionsHandle<'a> = crate::perf::NodeOptionsHandle<'a, MoveNode>;

pub type ChildMapK<'a> = (NodeHandle<'a>, u8, u8);
pub type ChildMapV<'a> = SliceHandle<'a, Node<'a>>;
// Node map type alias for clarity.
// key: (parent node address, s1_move_index, s2_move_index)
pub type ChildMap<'a> = DashMap<ChildMapK<'a>, ChildMapV<'a>, foldhash::fast::RandomState>;

fn sigmoid(x: f32) -> f32 {
    // Tuned so that ~200 points is very close to 1.0
    1.0 / (1.0 + (-0.0125 * x).exp())
}

pub struct MoveNode {
    move_choice: MoveChoice,
    total_score: AtomicU32,
    visits: AtomicU32,
}

impl MoveNode {
    fn new(move_choice: MoveChoice) -> Self {
        Self {
            move_choice,
            total_score: AtomicU32::new(0),
            visits: AtomicU32::new(0),
        }
    }

    fn add_virtual_loss(&self) {
        self.visits.fetch_add(VIRTUAL_LOSS_VISITS, Ordering::AcqRel);
    }

    fn remove_virtual_loss(&self) {
        self.visits.fetch_sub(VIRTUAL_LOSS_VISITS, Ordering::AcqRel);
    }

    fn add_result(&self, score: f32) {
        self.total_score
            .fetch_add((score * SCORE_SCALE).round() as u32, Ordering::AcqRel);
        self.visits.fetch_add(1, Ordering::AcqRel);
    }

    fn total_score_f32(&self) -> f32 {
        self.total_score.load(Ordering::Acquire) as f32 / SCORE_SCALE
    }

    fn ucb1(&self, parent_visits: u32) -> f32 {
        let visits = self.visits.load(Ordering::Acquire);
        if visits == 0 {
            return f32::INFINITY;
        }
        let average_score = self.total_score_f32() / visits as f32;
        let exploration = 2.0 * (parent_visits as f32).ln().max(0.0) / visits as f32;
        average_score + exploration.sqrt()
    }
}

fn sample_node<'a>(
    nodes: SliceHandle<'a, Node<'a>>,
    arena: &Arena<'a>,
    rng: &mut Rng,
) -> NodeHandle<'a> {
    if nodes.len() <= 1 {
        return nodes.iter().next().unwrap();
    }
    let mut prefix_sum = 0.;
    let roll = rng.random_range(0f32..100f32);
    for node in nodes.iter() {
        prefix_sum += node.resolve(arena).percentage.max(0.0);
        if prefix_sum >= roll {
            return node;
        }
    }
    nodes.iter().last().unwrap()
}

struct PathStep<'a> {
    parent: &'a Node<'a>,
    child: &'a Node<'a>,
    s1_index: u8,
    s2_index: u8,
}

pub struct Node<'a> {
    pub times_visited: AtomicU32,
    pub percentage: f32,
    pub instruction_list: SliceHandle<'a, Instruction>,
    virtual_losses: AtomicI8,
    pub options: OnceLock<NodeOptionsHandle<'a>>,
}

impl<'a> Node<'a> {
    fn new_root_in(
        arena: &mut Arena<'a>,
        s1_options: Vec<MoveChoice>,
        s2_options: Vec<MoveChoice>,
    ) -> Self {
        Self {
            times_visited: AtomicU32::new(0),
            percentage: 100.,
            instruction_list: unsafe { arena.alloc_slice([].iter().cloned()) },
            virtual_losses: AtomicI8::new(0),
            options: OnceLock::from(SharedNodeOptions::new_in(
                arena,
                &s1_options,
                &s2_options,
                MoveNode::new,
            )),
        }
    }

    fn new_child(percentage: f32, instruction_list: SliceHandle<'a, Instruction>) -> Self {
        Self {
            times_visited: AtomicU32::new(0),
            percentage,
            instruction_list,
            virtual_losses: AtomicI8::new(0),
            options: OnceLock::new(),
        }
    }

    fn ensure_options(&self, arena: &mut Arena<'a>, state: &State) -> &NodeOptionsHandle<'a> {
        self.options.get_or_init(|| {
            let (s1, s2) = state.get_all_options();
            SharedNodeOptions::new_in(arena, &s1, &s2, MoveNode::new)
        })
    }

    fn select_move_pair(&self, arena: &mut Arena<'a>, state: &State) -> (u8, u8) {
        let options = self.ensure_options(arena, state).resolve(arena);
        let parent_visits = self
            .times_visited
            .load(Ordering::Acquire)
            .saturating_add(self.virtual_losses.load(Ordering::Acquire).max(0) as u32)
            .max(1);
        (
            self.maximize_ucb_for_side(options.s1(), parent_visits),
            self.maximize_ucb_for_side(options.s2(), parent_visits),
        )
    }

    fn selection(
        root: NodeHandle<'a>,
        state: &mut State,
        rng: &mut Rng,
        children: &ChildMap<'a>,
        path: &mut Vec<PathStep<'a>>,
        arena: &mut Arena<'a>,
    ) -> (NodeHandle<'a>, u8, u8) {
        // raw pointers walk both the root (a standalone Arc<Node>) and children
        // (Nodes living inside a branch's Arc<[Node]>) uniformly. every node is
        // owned by children/root for the whole search, so the pointers stay
        // valid
        let mut current = root;
        loop {
            let (s1_index, s2_index) = current.resolve(arena).select_move_pair(arena, state);
            let options = current
                .resolve(arena)
                .options
                .get()
                .expect("options set during selection")
                .resolve(arena);

            let key = (current, s1_index, s2_index);
            match children.get(&key) {
                Some(branch) => {
                    let child = sample_node(*branch, arena, rng);

                    // drop the DashMap ref before mutating state to avoid
                    // holding the lock any longer than necessary. the sampled
                    // node stays alive via the branch's Arc<[Node]> in the
                    // ChildMap
                    drop(branch);

                    options.s1()[s1_index as usize].add_virtual_loss();
                    options.s2()[s2_index as usize].add_virtual_loss();
                    child
                        .resolve(arena)
                        .virtual_losses
                        .fetch_add(1, Ordering::AcqRel);
                    state.apply_instructions(child.resolve(arena).instruction_list.resolve(arena));
                    path.push(PathStep {
                        parent: current.resolve(arena),
                        child: child.resolve(arena),
                        s1_index,
                        s2_index,
                    });
                    current = child;
                }
                None => {
                    // this is the leaf, stop selection
                    return (current, s1_index, s2_index);
                }
            }
        }
    }

    fn maximize_ucb_for_side(&self, side_options: &[MoveNode], parent_visits: u32) -> u8 {
        side_options
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.ucb1(parent_visits)
                    .partial_cmp(&b.ucb1(parent_visits))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i as u8)
            .unwrap_or(0)
    }

    /// looks up or creates the child branch for `(s1_index, s2_index)` and
    /// returns one sampled child, applying virtual loss bookkeeping.  Returns
    /// `None` when the node should not be expanded (battle over, both-None).
    fn expand(
        leaf: NodeHandle<'a>,
        state: &mut State,
        s1_index: u8,
        s2_index: u8,
        rng: &mut Rng,
        children: &ChildMap<'a>,
        depth: usize,
        arena: &mut Arena<'a>,
    ) -> Option<NodeHandle<'a>> {
        let options = leaf
            .resolve(arena)
            .options
            .get()
            .expect("options initialised before expand")
            .resolve(arena);
        let s1_move = &options.s1()[s1_index as usize].move_choice;
        let s2_move = &options.s2()[s2_index as usize].move_choice;

        if (state.battle_is_over() != 0.0 && depth != 0)
            || (s1_move == &MoveChoice::None && s2_move == &MoveChoice::None)
        {
            return None;
        }

        let should_branch_on_damage = depth < MCTS_DAMAGE_BRANCH_DEPTH;
        let mut instructions =
            generate_instructions_from_move_pair(state, s1_move, s2_move, should_branch_on_damage);
        // put the most likely branches first
        instructions.sort_unstable_by(|l, r| l.percentage.total_cmp(&r.percentage).reverse());
        let instructions = instructions
            .into_iter()
            .map(|si| {
                (si.percentage, unsafe {
                    arena.alloc_slice(si.instruction_list.into_iter())
                })
            })
            .collect::<Vec<_>>();
        let nodes = unsafe {
            arena.alloc_slice(
                instructions
                    .into_iter()
                    .map(|si| Node::new_child(si.0, si.1)),
            )
        };
        let key = (leaf, s1_index, s2_index);
        // entry() on DashMap is atomic per-shard: only one thread will
        // construct the branch; all others get the winner's branch.
        let nodes_ref = children.entry(key).or_insert(nodes);

        Some(sample_node(*nodes_ref, arena, rng))
    }

    fn rollout(&self, state: &State, root_eval: f32) -> f32 {
        let battle_is_over = state.battle_is_over();
        if battle_is_over == 0.0 {
            sigmoid(evaluate(state) - root_eval)
        } else if battle_is_over == -1.0 {
            0.0
        } else {
            battle_is_over
        }
    }

    // walk `path` in reverse, updating visit counts and scores,
    // removes virtual losses, and reverse-applying instructions to restore `state` to how it
    // was in the root
    fn backpropagate(
        path: &[PathStep],
        leaf: &Node,
        score: f32,
        state: &mut State,
        arena: &Arena<'a>,
    ) {
        leaf.times_visited.fetch_add(1, Ordering::AcqRel);

        for step in path.iter().rev() {
            let (parent, child) = (step.parent, step.child);
            let options = parent
                .options
                .get()
                .expect("path parent has options")
                .resolve(arena);
            options.s1()[step.s1_index as usize].add_result(score);
            options.s1()[step.s1_index as usize].remove_virtual_loss();
            options.s2()[step.s2_index as usize].add_result(1.0 - score);
            options.s2()[step.s2_index as usize].remove_virtual_loss();
            parent.times_visited.fetch_add(1, Ordering::AcqRel);
            child.virtual_losses.fetch_sub(1, Ordering::AcqRel);
            state.reverse_instructions(child.instruction_list.resolve(arena));
        }
    }
}

fn do_mcts<'a>(
    root: NodeHandle<'a>,
    state: &mut State,
    root_eval: f32,
    rng: &mut Rng,
    children: &ChildMap<'a>,
    path: &mut Vec<PathStep<'a>>,
    arena: &mut Arena<'a>,
) {
    path.clear();

    let (leaf, s1_index, s2_index) = Node::selection(root, state, rng, children, path, arena);

    let options = leaf
        .resolve(arena)
        .options
        .get()
        .expect("options set during selection")
        .resolve(arena);
    options.s1()[s1_index as usize].add_virtual_loss();
    options.s2()[s2_index as usize].add_virtual_loss();
    let expanded = Node::expand(
        leaf,
        state,
        s1_index,
        s2_index,
        rng,
        children,
        path.len(),
        arena,
    );
    let rollout_target = match expanded {
        Some(child) => {
            child
                .resolve(arena)
                .virtual_losses
                .fetch_add(1, Ordering::AcqRel);
            state.apply_instructions(child.resolve(arena).instruction_list.resolve(arena));
            path.push(PathStep {
                parent: leaf.resolve(arena),
                child: child.resolve(arena),
                s1_index,
                s2_index,
            });
            child
        }

        // if expansion returns None,
        // the battle is either over or both sides have no valid moves
        // so no child is added to the tree
        // we do a rollout on the leaf and backpropagate without adding a child to the tree
        None => {
            // remove the virtual loss we added before expansion, since we're not actually expanding
            options.s1()[s1_index as usize].remove_virtual_loss();
            options.s2()[s2_index as usize].remove_virtual_loss();
            leaf
        }
    };
    let score = rollout_target.resolve(arena).rollout(state, root_eval);
    Node::backpropagate(path, rollout_target.resolve(arena), score, state, arena);
}

pub fn perform_mcts_shared_tree(
    state: &mut State,
    side_one_options: Vec<MoveChoice>,
    side_two_options: Vec<MoveChoice>,
    max_time: Duration,
    worker_count: usize,
) -> MctsResult {
    let base_arena = ArenaPool::new();
    let r = perform_mcts_shared_tree_inner(
        state,
        side_one_options,
        side_two_options,
        max_time,
        worker_count,
        &base_arena,
    )
    .0;
    r
}

pub fn perform_mcts_shared_tree_inner<'a>(
    state: &mut State,
    side_one_options: Vec<MoveChoice>,
    side_two_options: Vec<MoveChoice>,
    max_time: Duration,
    worker_count: usize,
    base_arena: &'a ArenaPool,
) -> (MctsResult, NodeHandle<'a>, ChildMap<'a>) {
    let root_eval = evaluate(state);
    let deadline = Instant::now() + max_time;
    let root: NodeHandle = {
        let mut a = base_arena.sub_arena();
        let node = Node::new_root_in(&mut a, side_one_options, side_two_options);
        a.alloc(node)
    };
    let started_iterations = AtomicU32::new(0);

    // global map shared by all threads.
    let children: ChildMap =
        DashMap::with_capacity_and_hasher(1 << 16, foldhash::fast::RandomState::default());

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let children = &children;
            let started_iterations = &started_iterations;
            let mut worker_state = state.clone();
            scope.spawn(move || {
                let mut rng = rand::rngs::SmallRng::from_rng(&mut rng());
                let mut path = Vec::with_capacity(16);
                let mut arena = base_arena.sub_arena();

                while Instant::now() < deadline {
                    for _ in 0..MCTS_DEADLINE_CHECK_INTERVAL {
                        do_mcts(
                            root,
                            &mut worker_state,
                            root_eval,
                            &mut rng,
                            &children,
                            &mut path,
                            &mut arena,
                        );
                        if started_iterations
                            .fetch_add(MCTS_DEADLINE_CHECK_INTERVAL, Ordering::AcqRel)
                            >= MCTS_MAX_ITERATIONS_PER_TREE
                        {
                            break;
                        }
                    }
                }
            });
        }
    });

    let options = root
        .resolve(&base_arena.sub_arena())
        .options
        .get()
        .expect("root options initialized")
        .resolve(&base_arena.sub_arena());
    let result = MctsResult {
        s1: options
            .s1()
            .iter()
            .map(|v| MctsSideResult {
                move_choice: v.move_choice,
                total_score: v.total_score_f32(),
                visits: v.visits.load(Ordering::Acquire),
            })
            .collect(),
        s2: options
            .s2()
            .iter()
            .map(|v| MctsSideResult {
                move_choice: v.move_choice,
                total_score: v.total_score_f32(),
                visits: v.visits.load(Ordering::Acquire),
            })
            .collect(),
        iteration_count: root
            .resolve(&base_arena.sub_arena())
            .times_visited
            .load(Ordering::Acquire),
    };
    (result, root, children)
}
