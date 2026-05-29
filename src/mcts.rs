use crate::engine::evaluate::evaluate;
use crate::engine::generate_instructions::generate_instructions_from_move_pair;
use crate::engine::state::MoveChoice;
use crate::instruction::StateInstructions;
use crate::perf::Timers;
use crate::state::State;
use rand::distr::weighted::WeightedIndex;
use rand::prelude::*;
use rand::rng;
use std::cell::Cell;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

fn sigmoid(x: f32) -> f32 {
    // Tuned so that ~200 points is very close to 1.0
    1.0 / (1.0 + (-0.0125 * x).exp())
}

pub type NodeOptions = crate::perf::NodeOptions<MoveNode>;

pub type ChildMapK = (usize, u8, u8);
pub type ChildMapV = Box<[Node]>;
pub type ChildMap = HashMap<ChildMapK, ChildMapV>;

#[derive(Debug)]
pub struct Node {
    pub root: bool,
    pub parent: *const Node,
    pub times_visited: Cell<u32>,

    // represents the instructions & s1/s2 moves that led to this node from the parent
    pub instructions: StateInstructions,
    pub s1_choice: u8,
    pub s2_choice: u8,

    /// represents the total score and number of visits for this node
    pub options: OnceCell<NodeOptions>,
}

impl Node {
    fn new() -> Node {
        Node {
            root: false,
            parent: std::ptr::null_mut(),
            instructions: StateInstructions::default(),
            times_visited: Cell::new(0),
            s1_choice: 0,
            s2_choice: 0,
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

    pub unsafe fn selection(
        &self,
        state: &mut State,
        children: &mut ChildMap,
    ) -> (*const Node, u8, u8) {
        self.options.get_or_init(|| {
            let (s1_options, s2_options) = state.get_all_options();
            NodeOptions::new(&s1_options, &s2_options, MoveNode::from_move_choice)
        });

        let s1_mc_index = self.maximize_ucb_for_side(self.options.get().unwrap().s1());
        let s2_mc_index = self.maximize_ucb_for_side(self.options.get().unwrap().s2());
        let key = (self as *const Node as usize, s1_mc_index, s2_mc_index);
        match children.get_mut(&key) {
            Some(child_slice) => {
                let child_slice_ptr = child_slice as *const Box<[Node]>;
                let chosen_child = self.sample_node(child_slice_ptr);
                state.apply_instructions(&(*chosen_child).instructions.instruction_list);
                (*chosen_child).selection(state, children)
            }
            None => (self as *const Node, s1_mc_index, s2_mc_index),
        }
    }

    unsafe fn sample_node(&self, move_vector: *const Box<[Node]>) -> *const Node {
        let mut rng = rng();
        let weights: Vec<f64> = (*move_vector)
            .iter()
            .map(|x| x.instructions.percentage as f64)
            .collect();
        let dist = WeightedIndex::new(weights).unwrap();
        let chosen_node = &(&*move_vector)[dist.sample(&mut rng)];
        let chosen_node_ptr = chosen_node as *const Node;
        chosen_node_ptr
    }

    pub unsafe fn expand(
        &self,
        state: &mut State,
        s1_move_index: u8,
        s2_move_index: u8,
        children: &mut ChildMap,
    ) -> *const Node {
        let s1_move = &self.options.get().unwrap().s1()[s1_move_index as usize].move_choice;
        let s2_move = &self.options.get().unwrap().s2()[s2_move_index as usize].move_choice;
        // if the battle is over or both moves are none there is no need to expand
        if (state.battle_is_over() != 0.0 && !self.root)
            || (s1_move == &MoveChoice::None && s2_move == &MoveChoice::None)
        {
            return self as *const Node;
        }
        let should_branch_on_damage = self.root || (*self.parent).root;
        let new_instructions =
            generate_instructions_from_move_pair(state, s1_move, s2_move, should_branch_on_damage);
        let mut this_pair_slice = new_instructions
            .into_iter()
            .map(|mut state_instructions| {
                let mut new_node = Node::new();
                new_node.parent = self;
                state_instructions.instruction_list.shrink_to_fit();
                new_node.instructions = state_instructions;
                new_node.s1_choice = s1_move_index as u8;
                new_node.s2_choice = s2_move_index as u8;
                new_node
            })
            .collect::<Box<[Node]>>();

        // sample a node from the new instruction list.
        // this is the node that the rollout will be done on
        let new_node_ptr = self.sample_node(&mut this_pair_slice);
        state.apply_instructions(&(*new_node_ptr).instructions.instruction_list);

        let key = (self as *const Node as usize, s1_move_index, s2_move_index);
        children.insert(key, this_pair_slice);
        new_node_ptr
    }

    pub unsafe fn backpropagate(&self, score: f32, state: &mut State) {
        self.times_visited.update(|v| v + 1);
        if self.root {
            return;
        }

        let parent_s1_movenode =
            &(*self.parent).options.get().unwrap().s1()[self.s1_choice as usize];
        parent_s1_movenode.total_score.update(|v| v + score);
        parent_s1_movenode.visits.update(|v| v + 1);

        let parent_s2_movenode =
            &(*self.parent).options.get().unwrap().s2()[self.s2_choice as usize];
        parent_s2_movenode.total_score.update(|v| v + 1.0 - score);
        parent_s2_movenode.visits.update(|v| v + 1);

        state.reverse_instructions(&self.instructions.instruction_list);
        (*self.parent).backpropagate(score, state);
    }

    pub fn rollout(&self, state: &mut State, root_eval: &f32) -> f32 {
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
    timers: &mut Timers,
) {
    let t0 = Instant::now();
    let (mut new_node, s1_move, s2_move) = unsafe { root_node.selection(state, children) };
    let t1 = Instant::now();
    new_node = unsafe { (*new_node).expand(state, s1_move, s2_move, children) };
    let t2 = Instant::now();
    let rollout_result = unsafe { (*new_node).rollout(state, root_eval) };
    let t3 = Instant::now();
    unsafe { (*new_node).backpropagate(rollout_result, state) }
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
    root_node.root = true;
    let mut children: ChildMap = ChildMap::new();

    let root_eval = evaluate(state);
    let start_time = Instant::now();
    while start_time.elapsed() < max_time {
        for _ in 0..1000 {
            do_mcts(
                &mut root_node,
                state,
                &root_eval,
                &mut children,
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
