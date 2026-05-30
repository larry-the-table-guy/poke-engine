use crate::engine::evaluate::evaluate;
use crate::engine::generate_instructions::generate_instructions_from_move_pair;
use crate::engine::state::MoveChoice;
use crate::instruction::StateInstructions;
use crate::perf::Timers;
use crate::state::State;
use foldhash::{HashMap, HashMapExt};
use rand::prelude::*;
use rand::rng;
use std::cell::Cell;
use std::cell::OnceCell;
use std::time::{Duration, Instant};

fn sigmoid(x: f32) -> f32 {
    // Tuned so that ~200 points is very close to 1.0
    1.0 / (1.0 + (-0.0125 * x).exp())
}

const MCTS_DAMAGE_BRANCH_DEPTH: usize = 2;

pub type NodeOptions = crate::perf::NodeOptions<MoveNode>;

pub type ChildMapK = (usize, u8, u8);
pub type ChildMapV = Box<[Node]>;
pub type ChildMap = HashMap<ChildMapK, ChildMapV>;

#[derive(Debug)]
pub struct Node {
    pub times_visited: Cell<u32>,

    // represents the instructions that led to this node from the parent
    pub instructions: StateInstructions,

    /// represents the total score and number of visits for this node
    pub options: OnceCell<NodeOptions>,
}

impl Node {
    fn new() -> Node {
        Node {
            instructions: StateInstructions::default(),
            times_visited: Cell::new(0),
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
        root: &Node,
        state: &mut State,
        children: &mut ChildMap,
        path: &mut Vec<PathStep>,
    ) -> (*const Node, u8, u8) {
        let mut current: *const Node = root;
        loop {
            let node = unsafe { &*current };
            node.options.get_or_init(|| {
                let (s1_options, s2_options) = state.get_all_options();
                NodeOptions::new(&s1_options, &s2_options, MoveNode::from_move_choice)
            });
            let s1_index = node.maximize_ucb_for_side(node.options.get().unwrap().s1());
            let s2_index = node.maximize_ucb_for_side(node.options.get().unwrap().s2());
            let key = (node as *const Node as usize, s1_index, s2_index);

            match children.get_mut(&key) {
                Some(child_slice) => {
                    let chosen_child = Node::sample_node(child_slice);
                    state.apply_instructions(&chosen_child.instructions.instruction_list);
                    path.push(PathStep {
                        parent: current,
                        child: chosen_child,
                        s1_index,
                        s2_index,
                    });
                    current = chosen_child;
                }
                None => break (current, s1_index, s2_index),
            }
        }
    }

    fn sample_node(moves: &[Node]) -> &Node {
        let roll = rng().random_range(0f32..100f32);
        let mut prefix_sum = 0f32;
        for m in moves {
            prefix_sum += m.instructions.percentage;
            if prefix_sum >= roll {
                return m;
            }
        }
        moves.last().unwrap()
    }

    fn expand(
        &self,
        state: &mut State,
        s1_move_index: u8,
        s2_move_index: u8,
        children: &mut ChildMap,
        depth: usize,
    ) -> Option<*const Node> {
        let s1_move = &self.options.get().unwrap().s1()[s1_move_index as usize].move_choice;
        let s2_move = &self.options.get().unwrap().s2()[s2_move_index as usize].move_choice;
        // if the battle is over or both moves are none there is no need to expand
        if (state.battle_is_over() != 0.0 && depth != 0)
            || (s1_move == &MoveChoice::None && s2_move == &MoveChoice::None)
        {
            return None;
        }
        let should_branch_on_damage = depth < MCTS_DAMAGE_BRANCH_DEPTH;
        let new_instructions =
            generate_instructions_from_move_pair(state, s1_move, s2_move, should_branch_on_damage);
        let this_pair_slice = new_instructions
            .into_iter()
            .map(|mut state_instructions| {
                let mut new_node = Node::new();
                state_instructions.instruction_list.shrink_to_fit();
                new_node.instructions = state_instructions;
                new_node
            })
            .collect::<Box<[Node]>>();

        // sample a node from the new instruction list.
        // this is the node that the rollout will be done on
        let new_node_ptr = Node::sample_node(&this_pair_slice) as *const Node;
        let key = (self as *const Node as usize, s1_move_index, s2_move_index);
        children.insert(key, this_pair_slice);
        Some(new_node_ptr)
    }

    fn backpropagate(path: &[PathStep], leaf: &Node, score: f32, state: &mut State) {
        leaf.times_visited.update(|v| v + 1);
        for step in path.iter().rev() {
            let (parent, child) = unsafe { (&*step.parent, &*step.child) };
            let options = parent.options.get().expect("path parent has options");

            let parent_s1_movenode = &options.s1()[step.s1_index as usize];
            parent_s1_movenode.total_score.update(|v| v + score);
            parent_s1_movenode.visits.update(|v| v + 1);

            let parent_s2_movenode = &options.s2()[step.s2_index as usize];
            parent_s2_movenode.total_score.update(|v| v + 1.0 - score);
            parent_s2_movenode.visits.update(|v| v + 1);

            parent.times_visited.update(|v| v + 1);

            state.reverse_instructions(&child.instructions.instruction_list);
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

struct PathStep {
    parent: *const Node,
    child: *const Node,
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

fn do_mcts(
    root_node: &Node,
    state: &mut State,
    root_eval: &f32,
    children: &mut ChildMap,
    path: &mut Vec<PathStep>,
    timers: &mut Timers,
) {
    path.clear();
    let t0 = Instant::now();
    let (leaf, s1_index, s2_index) = Node::selection(root_node, state, children, path);
    let t1 = Instant::now();
    let expanded = unsafe { &*leaf }.expand(state, s1_index, s2_index, children, path.len());
    let rollout_target = if let Some(child) = expanded {
        let child = unsafe { &*child };
        state.apply_instructions(&child.instructions.instruction_list);
        path.push(PathStep {
            parent: leaf,
            child,
            s1_index,
            s2_index,
        });
        child
    } else {
        leaf
    };
    let t2 = Instant::now();
    let rollout_result = Node::rollout(state, root_eval);
    let t3 = Instant::now();
    Node::backpropagate(path, unsafe { &*rollout_target }, rollout_result, state);
    let t4 = Instant::now();
    timers.selection += t1.duration_since(t0).as_nanos() as u64;
    timers.expand += t2.duration_since(t1).as_nanos() as u64;
    timers.rollout += t3.duration_since(t2).as_nanos() as u64;
    timers.backpropagate += t4.duration_since(t3).as_nanos() as u64;
}

pub fn perform_mcts(
    state: &mut State,
    side_one_options: Vec<MoveChoice>,
    side_two_options: Vec<MoveChoice>,
    max_time: Duration,
) -> MctsResult {
    perform_mcts_inner(state, side_one_options, side_two_options, max_time).0
}

pub fn perform_mcts_inner(
    state: &mut State,
    side_one_options: Vec<MoveChoice>,
    side_two_options: Vec<MoveChoice>,
    max_time: Duration,
) -> (MctsResult, Box<Node>, Timers, ChildMap) {
    let mut timers = Timers::default();
    let mut root_node = Box::new(Node::new());
    root_node.options = OnceCell::from(NodeOptions::new(
        &side_one_options,
        &side_two_options,
        MoveNode::from_move_choice,
    ));
    let mut children: ChildMap = ChildMap::new();
    let mut path = Vec::with_capacity(16);

    let root_eval = evaluate(state);
    let start_time = Instant::now();
    while start_time.elapsed() < max_time {
        for _ in 0..1000 {
            do_mcts(
                &mut root_node,
                state,
                &root_eval,
                &mut children,
                &mut path,
                &mut timers,
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
        if root_node.times_visited.get() == 10_000_000 {
            break;
        }
    }
    let result = MctsResult {
        s1: root_node
            .options
            .get()
            .unwrap()
            .s1()
            .iter()
            .map(|v| MctsSideResult {
                move_choice: v.move_choice.clone(),
                total_score: v.total_score.get(),
                visits: v.visits.get(),
            })
            .collect(),
        s2: root_node
            .options
            .get()
            .unwrap()
            .s2()
            .iter()
            .map(|v| MctsSideResult {
                move_choice: v.move_choice.clone(),
                total_score: v.total_score.get(),
                visits: v.visits.get(),
            })
            .collect(),
        iteration_count: root_node.times_visited.get(),
    };

    (result, root_node, timers, children)
}
