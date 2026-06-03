use crate::engine::evaluate::evaluate;
use crate::engine::generate_instructions::generate_instructions_from_move_pair;
use crate::engine::state::MoveChoice;
use crate::instruction::Instruction;
use crate::perf::arena::{Arena, Handle, SliceHandle};
use crate::state::State;
use foldhash::{HashMap, HashMapExt};
use rand::{prelude::*, rng, rngs::SmallRng as Rng, Rng as _};
use std::cell::{Cell, OnceCell};
use std::time::{Duration, Instant};

fn sigmoid(x: f32) -> f32 {
    // Tuned so that ~200 points is very close to 1.0
    1.0 / (1.0 + (-0.0125 * x).exp())
}

const MCTS_DAMAGE_BRANCH_DEPTH: usize = 2;

type NodeHandle<'arena> = Handle<'arena, Node<'arena>>;
pub type NodeOptions<'arena> = crate::perf::NodeOptions<'arena, MoveNode>;
pub type NodeOptionsHandle<'arena> = crate::perf::NodeOptionsHandle<'arena, MoveNode>;

pub type ChildMapK<'arena> = (NodeHandle<'arena>, u8, u8);
pub type ChildMapV<'arena> = SliceHandle<'arena, Node<'arena>>;
pub type ChildMap<'arena> = HashMap<ChildMapK<'arena>, ChildMapV<'arena>>;

pub struct Node<'arena> {
    pub times_visited: Cell<u32>,

    /// How likely this node was as a result of the parent.
    pub percentage: f32,
    // represents the instructions that led to this node from the parent
    pub instruction_list: SliceHandle<'arena, Instruction>,

    /// represents the total score and number of visits for this node
    pub options: OnceCell<NodeOptionsHandle<'arena>>,
}

impl<'arena> Node<'arena> {
    fn new(percentage: f32, instruction_list: SliceHandle<'arena, Instruction>) -> Node<'arena> {
        Node {
            times_visited: Cell::new(0),
            instruction_list,
            percentage,
            options: OnceCell::new(),
        }
    }

    pub fn maximize_ucb_for_side(&self, side_map: &[MoveNode]) -> u8 {
        let mut choice = 0;
        let mut best_ucb1 = f32::MIN;
        for (index, node) in side_map.iter().enumerate() {
            let this_ucb1 = node.ucb1(self.times_visited.get());
            if this_ucb1 > best_ucb1 {
                best_ucb1 = this_ucb1;
                choice = index;
            }
        }
        choice as u8
    }

    fn selection(
        root: NodeHandle<'arena>,
        state: &mut State,
        children: &mut ChildMap<'arena>,
        path: &mut Vec<PathStep<'arena>>,
        rng: &mut Rng,
        arena: &mut Arena<'arena>,
    ) -> (NodeHandle<'arena>, u8, u8) {
        let mut current = root;
        loop {
            let node = current.resolve(arena);
            node.options.get_or_init(|| {
                let (s1_options, s2_options) = state.get_all_options();
                NodeOptions::new_in(arena, &s1_options, &s2_options, MoveNode::from_move_choice)
            });
            let options = node.options.get().unwrap().resolve(arena);
            let s1_index = node.maximize_ucb_for_side(options.s1());
            let s2_index = node.maximize_ucb_for_side(options.s2());
            let key = (current, s1_index, s2_index);

            match children.get(&key) {
                Some(child_slice) => {
                    let chosen_child = Node::sample_node(rng, arena, *child_slice);
                    state.apply_instructions(
                        chosen_child.resolve(arena).instruction_list.resolve(arena),
                    );
                    path.push(PathStep {
                        parent: node,
                        child: chosen_child.resolve(arena),
                        s1_index,
                        s2_index,
                    });
                    current = chosen_child;
                }
                None => break (current, s1_index, s2_index),
            }
        }
    }

    fn sample_node(
        rng: &mut Rng,
        arena: &Arena<'arena>,
        nodes: SliceHandle<'arena, Node<'arena>>,
    ) -> NodeHandle<'arena> {
        if nodes.len() == 1 {
            return nodes.iter().next().unwrap();
        }
        let roll = rng.random_range(0f32..100f32);
        let mut prefix_sum = 0f32;
        for m in nodes.iter() {
            prefix_sum += m.resolve(arena).percentage;
            if prefix_sum >= roll {
                return m;
            }
        }
        nodes.iter().last().unwrap()
    }

    fn expand(
        leaf: NodeHandle<'arena>,
        state: &mut State,
        s1_move_index: u8,
        s2_move_index: u8,
        children: &mut ChildMap<'arena>,
        depth: usize,
        rng: &mut Rng,
        arena: &mut Arena<'arena>,
    ) -> Option<NodeHandle<'arena>> {
        let options = leaf.resolve(arena).options.get().unwrap().resolve(arena);
        let s1_move = &options.s1()[s1_move_index as usize].move_choice;
        let s2_move = &options.s2()[s2_move_index as usize].move_choice;
        // if the battle is over or both moves are none there is no need to expand
        if (state.battle_is_over() != 0.0 && depth != 0)
            || (s1_move == &MoveChoice::None && s2_move == &MoveChoice::None)
        {
            return None;
        }
        let should_branch_on_damage = depth < MCTS_DAMAGE_BRANCH_DEPTH;
        let mut new_instructions =
            generate_instructions_from_move_pair(state, s1_move, s2_move, should_branch_on_damage);
        // put the most likely branches first
        new_instructions.sort_unstable_by(|l, r| l.percentage.total_cmp(&r.percentage).reverse());
        let collect = new_instructions
            .into_iter()
            .map(|si| {
                (
                    si.percentage,
                    unsafe { arena.alloc_slice(si.instruction_list.into_iter()) },
                )
            })
            .collect::<Vec<_>>() // TODO: remove when Node becomes DST
            ;
        let this_pair_slice =
            unsafe { arena.alloc_slice(collect.into_iter().map(|si| Node::new(si.0, si.1))) };
        // sample a node from the new instruction list.
        // this is the node that the rollout will be done on
        let new_node_ptr = Node::sample_node(rng, arena, this_pair_slice);
        let key = (leaf, s1_move_index, s2_move_index);
        children.insert(key, this_pair_slice);
        Some(new_node_ptr)
    }

    fn backpropagate(
        path: &[PathStep],
        leaf: &Node,
        score: f32,
        state: &mut State,
        arena: &mut Arena<'arena>,
    ) {
        leaf.times_visited.update(|v| v + 1);
        for step in path.iter().rev() {
            let (parent, child) = (step.parent, step.child);
            let options = parent
                .options
                .get()
                .expect("path parent has options")
                .resolve(arena);

            let parent_s1_movenode = &options.s1()[step.s1_index as usize];
            parent_s1_movenode.total_score.update(|v| v + score);
            parent_s1_movenode.visits.update(|v| v + 1);

            let parent_s2_movenode = &options.s2()[step.s2_index as usize];
            parent_s2_movenode.total_score.update(|v| v + 1.0 - score);
            parent_s2_movenode.visits.update(|v| v + 1);

            parent.times_visited.update(|v| v + 1);

            state.reverse_instructions(&child.instruction_list.resolve(arena));
        }
    }

    fn rollout(state: &mut State, root_eval: &f32) -> f32 {
        let battle_is_over = state.battle_is_over();
        if battle_is_over == 0.0 {
            let eval = evaluate(state);
            sigmoid(eval - root_eval)
        } else {
            if battle_is_over == -1.0 {
                0.0
            } else {
                battle_is_over
            }
        }
    }
}

struct PathStep<'a> {
    parent: &'a Node<'a>,
    child: &'a Node<'a>,
    s1_index: u8,
    s2_index: u8,
}

#[derive(Debug)]
pub struct MoveNode {
    pub move_choice: MoveChoice,
    pub total_score: Cell<f32>,
    pub visits: Cell<u32>,
}

impl MoveNode {
    pub fn ucb1(&self, parent_visits: u32) -> f32 {
        if self.visits.get() == 0 {
            return f32::INFINITY;
        }
        let score = (self.total_score.get() / self.visits.get() as f32)
            + (2.0 * (parent_visits as f32).ln() / self.visits.get() as f32).sqrt();
        score
    }
    pub fn average_score(&self) -> f32 {
        let score = self.total_score.get() / self.visits.get() as f32;
        score
    }
    fn from_move_choice(move_choice: MoveChoice) -> Self {
        Self {
            move_choice: move_choice,
            total_score: Cell::new(0.),
            visits: Cell::new(0),
        }
    }
}

#[derive(Clone)]
pub struct MctsSideResult {
    pub move_choice: MoveChoice,
    pub total_score: f32,
    pub visits: u32,
}

impl MctsSideResult {
    pub fn average_score(&self) -> f32 {
        if self.visits == 0 {
            return 0.0;
        }
        let score = self.total_score / self.visits as f32;
        score
    }
}

pub struct MctsResult {
    pub s1: Vec<MctsSideResult>,
    pub s2: Vec<MctsSideResult>,
    pub iteration_count: u32,
}

fn do_mcts<'arena>(
    root_node: NodeHandle<'arena>,
    state: &mut State,
    root_eval: &f32,
    children: &mut ChildMap<'arena>,
    path: &mut Vec<PathStep<'arena>>,
    rng: &mut Rng,
    arena: &mut Arena<'arena>,
) {
    path.clear();
    let (leaf, s1_index, s2_index) = Node::selection(root_node, state, children, path, rng, arena);
    let expanded = Node::expand(
        leaf,
        state,
        s1_index,
        s2_index,
        children,
        path.len(),
        rng,
        arena,
    );
    let rollout_target = if let Some(child) = expanded {
        state.apply_instructions(&child.resolve(arena).instruction_list.resolve(arena));
        path.push(PathStep {
            parent: leaf.resolve(arena),
            child: child.resolve(arena),
            s1_index,
            s2_index,
        });
        child
    } else {
        leaf
    };
    let rollout_result = Node::rollout(state, root_eval);
    Node::backpropagate(
        path,
        rollout_target.resolve(arena),
        rollout_result,
        state,
        arena,
    );
}

pub fn perform_mcts(
    state: &mut State,
    side_one_options: Vec<MoveChoice>,
    side_two_options: Vec<MoveChoice>,
    max_time: Duration,
) -> MctsResult {
    let a = crate::perf::arena::ConcurrentArena::new();
    perform_mcts_inner(
        state,
        side_one_options,
        side_two_options,
        max_time,
        &mut a.sub_arena(),
    )
    .0
}

pub fn perform_mcts_inner<'a>(
    state: &mut State,
    side_one_options: Vec<MoveChoice>,
    side_two_options: Vec<MoveChoice>,
    max_time: Duration,
    arena: &mut Arena<'a>,
) -> (MctsResult, NodeHandle<'a>, ChildMap<'a>) {
    let root_node = {
        let s = unsafe { arena.alloc_slice([].iter().cloned()) };
        arena.alloc(Node::new(100., s))
    };
    let _ = root_node.resolve(arena).options.set(NodeOptions::new_in(
        arena,
        &side_one_options,
        &side_two_options,
        MoveNode::from_move_choice,
    ));
    let mut children: ChildMap = ChildMap::new();
    let mut path = Vec::with_capacity(16);
    let mut rng = Rng::from_rng(&mut rng());

    let root_eval = evaluate(state);
    let start_time = Instant::now();
    while start_time.elapsed() < max_time {
        for _ in 0..1000 {
            do_mcts(
                root_node,
                state,
                &root_eval,
                &mut children,
                &mut path,
                &mut rng,
                arena,
            );
        }

        /*
        Cut off after 10 million iterations

        Under normal circumstances the bot will only run for 2.5-3.5 million iterations
        however towards the end of a battle the bot may perform tens of millions of iterations

        Beyond about 30 million iterations some floating point nonsense happens where
        MoveNode.total_score stops updating because f32 does not have enough precision

        I can push the problem farther out by using f64 but if the bot is running for 10 million iterations
        then it almost certainly sees a forced win
        */
        if root_node.resolve(arena).times_visited.get() == 10_000_000 {
            break;
        }
    }
    let result = MctsResult {
        s1: root_node
            .resolve(arena)
            .options
            .get()
            .unwrap()
            .resolve(arena)
            .s1()
            .iter()
            .map(|v| MctsSideResult {
                move_choice: v.move_choice.clone(),
                total_score: v.total_score.get(),
                visits: v.visits.get(),
            })
            .collect(),
        s2: root_node
            .resolve(arena)
            .options
            .get()
            .unwrap()
            .resolve(arena)
            .s2()
            .iter()
            .map(|v| MctsSideResult {
                move_choice: v.move_choice.clone(),
                total_score: v.total_score.get(),
                visits: v.visits.get(),
            })
            .collect(),
        iteration_count: root_node.resolve(arena).times_visited.get(),
    };

    (result, root_node, children)
}
