// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

mod anticone_cache;
mod confirmation;
mod consensus_executor;
mod debug;

use self::debug::*;
use super::consensus::consensus_executor::ConsensusExecutor;
use crate::{
    block_data_manager::BlockDataManager,
    consensus::{
        anticone_cache::AnticoneCache,
        confirmation::ConfirmationTrait,
        consensus_executor::{EpochExecutionTask, RewardExecutionInfo},
    },
    db::COL_MISC,
    hash::KECCAK_EMPTY_LIST_RLP,
    pow::ProofOfWorkConfig,
    state::State,
    statedb::StateDb,
    statistics::SharedStatistics,
    storage::{state::StateTrait, StorageManagerTrait},
    transaction_pool::SharedTransactionPool,
    vm_factory::VmFactory,
};
use cfx_types::{into_i128, into_u256, Bloom, H160, H256, U256, U512};
// use fenwick_tree::FenwickTree;
use crate::pow::target_difficulty;
use hibitset::{BitSet, BitSetLike, DrainableBitSet};
use link_cut_tree::MinLinkCutTree;
use parking_lot::RwLock;
use primitives::{
    filter::{Filter, FilterError},
    log_entry::{LocalizedLogEntry, LogEntry},
    receipt::Receipt,
    Block, BlockHeaderBuilder, EpochNumber, SignedTransaction, StateRoot,
    StateRootAuxInfo, StateRootWithAuxInfo, TransactionAddress,
};
use rayon::prelude::*;
use rlp::*;
use slab::Slab;
use std::{
    cmp::{max, min},
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
    sync::Arc,
    thread::sleep,
    time::Duration,
};

const MIN_MAINTAINED_RISK: f64 = 0.000001;
const MAX_NUM_MAINTAINED_RISK: usize = 10;

pub const DEFERRED_STATE_EPOCH_COUNT: u64 = 5;

/// `REWARD_EPOCH_COUNT` needs to be larger than
/// `ANTICONE_PENALTY_UPPER_EPOCH_COUNT`. If we cannot cache receipts of recent
/// `REWARD_EPOCH_COUNT` epochs, the receipts will be loaded from db, which may
/// lead to performance downgrade
const REWARD_EPOCH_COUNT: u64 = 12;
const ANTICONE_PENALTY_UPPER_EPOCH_COUNT: u64 = 10;
const ANTICONE_PENALTY_RATIO: u64 = 100;
/// 900 Conflux tokens
const BASE_MINING_REWARD: u64 = 900;
/// The unit of one Conflux token: 10 ** 18
const CONFLUX_TOKEN: u64 = 1_000_000_000_000_000_000;
const GAS_PRICE_BLOCK_SAMPLE_SIZE: usize = 100;
const GAS_PRICE_TRANSACTION_SAMPLE_SIZE: usize = 10000;

pub const ADAPTIVE_WEIGHT_DEFAULT_ALPHA_NUM: u64 = 2;
pub const ADAPTIVE_WEIGHT_DEFAULT_ALPHA_DEN: u64 = 3;
pub const ADAPTIVE_WEIGHT_DEFAULT_BETA: u64 = 1000;
pub const HEAVY_BLOCK_DEFAULT_DIFFICULTY_RATIO: u64 = 240;

const NULL: usize = !0;

// This is the cap of the size of the anticone barrier. If we have more than
// this number we will use the brute_force O(n) algorithm instead.
const ANTICONE_BARRIER_CAP: usize = 1000;
// The number of epochs per era. Each era is a potential checkpoint position.
// The parent_edge checking and adaptive checking are defined relative to the
// era start blocks.
pub const ERA_EPOCH_COUNT: usize = 10000;

#[derive(Copy, Clone)]
pub struct ConsensusInnerConfig {
    // num/den is the actual adaptive alpha parameter in GHAST. We use a
    // fraction to get around the floating point problem
    pub adaptive_weight_alpha_num: u64,
    pub adaptive_weight_alpha_den: u64,
    // Beta is the threshold in GHAST algorithm
    pub adaptive_weight_beta: u64,
    // The heavy block ratio (h) in GHAST algorithm
    pub heavy_block_difficulty_ratio: u64,
    // Optimistic execution is the feature to execute ahead of the deferred
    // execution boundary. The goal is to pipeline the transaction
    // execution and the block packaging and verification.
    // optimistic_executed_height is the number of step to go ahead
    pub enable_optimistic_execution: bool,
}

pub struct ConsensusConfig {
    // If we hit invalid state root, we will dump the information into a
    // directory specified here. This is useful for testing.
    pub debug_dump_dir_invalid_state_root: String,
    // When bench_mode is true, the PoW solution verification will be skipped.
    // The transaction execution will also be skipped and only return the
    // pair of (KECCAK_NULL_RLP, KECCAK_EMPTY_LIST_RLP) This is for testing
    // only
    pub bench_mode: bool,
    // The configuration used by inner data
    pub inner_conf: ConsensusInnerConfig,
}

#[derive(Debug)]
pub struct ConsensusGraphStatistics {
    pub inserted_block_count: usize,
}

impl ConsensusGraphStatistics {
    pub fn new() -> ConsensusGraphStatistics {
        ConsensusGraphStatistics {
            inserted_block_count: 0,
        }
    }
}

pub struct ConsensusGraphNodeData {
    pub epoch_number: usize,
    pub partial_invalid: bool,
    /// The indices set of the blocks in the epoch when the current
    /// block is as pivot chain block. This set does not contain
    /// the block itself.
    pub blockset_in_own_view_of_epoch: HashSet<usize>,
    /// Ordered blocks in this epoch
    pub ordered_epoch_blocks: Vec<usize>,
    /// The minimum/maximum epoch number of the block in the view of other
    /// blocks including itself.
    pub min_epoch_in_other_views: u64,
    pub max_epoch_in_other_views: u64,
    pub sequence_number: u64,
}

impl ConsensusGraphNodeData {
    pub fn new(epoch_number: usize, height: u64, sequence_number: u64) -> Self {
        ConsensusGraphNodeData {
            epoch_number,
            partial_invalid: false,
            blockset_in_own_view_of_epoch: Default::default(),
            ordered_epoch_blocks: Default::default(),
            min_epoch_in_other_views: height,
            max_epoch_in_other_views: height,
            sequence_number,
        }
    }
}

pub struct ConsensusGraphPivotData {
    /// The set of blocks whose last_pivot_in_past point to this pivot chain
    /// location
    pub last_pivot_in_past_blocks: HashSet<usize>,
}

impl Default for ConsensusGraphPivotData {
    fn default() -> Self {
        ConsensusGraphPivotData {
            last_pivot_in_past_blocks: HashSet::new(),
        }
    }
}

///
/// Implementation details of the GHAST algorithm
///
/// Conflux uses the Greedy Heaviest Adaptive SubTree (GHAST) algorithm to
/// select a chain from the genesis block to one of the leaf blocks as the pivot
/// chain. For each block b, GHAST algorithm computes two values: stable and
/// adaptive. Let's take stable as an example:
///
/// 1   B = Past(b)
/// 2   a = b.parent
/// 3   stable = True
/// 4   Let f(x) = PastW(b) - PastW(x.parent) - x.parent.weight
/// 5   Let g(x) = SubTW(B, x)
/// 6   while a.parent != Nil do
/// 7       if f(a) > beta and g(a) / f(a) < alpha then
/// 8           stable = False
/// 9       a = a.parent
///
/// To efficiently compute stable, we maintain a link-cut tree called
/// stable_tree.
///
/// Assume alpha = n / d, then g(a) / f(a) < n / d
///   => d * g(a) < n * f(a)
///   => d * SubTW(B, x) < n * (PastW(b) - PastW(x.parent) - x.parent.weight)
///   => d * SubTW(B, x) + n * PastW(x.parent) + n * x.parent.weight < n *
/// PastW(b)
///
/// Note that for a given block b, PastW(b) is a constant,
/// so in order to calculate stable, it is suffice to calculate
/// argmin{d * SubTW(B, x) + n * x.parent.weight + n * PastW(x.parent)}.
/// Therefore, in the stable_tree, the value for x is
/// d * SubTW(B, x) + n * x.parent.weight + n * PastW(x.parent).
///
/// adaptive could be computed in a similar manner:
///
/// 1   B = Past(b)
/// 2   a = b.parent
/// 3   Let f(x) = SubTW(B, x.parent)
/// 4   Let g(x) = SubStableTW(B, x)
/// 5   adaptive = False
/// 6   while a.parent != Nil do
/// 7       if f(a) > beta and g(a) / f(a) < alpha then
/// 8           adaptive = True
/// 9       a = a.parent
///
/// The only difference is that when maintaining g(x) * d - f(x) * n, we need to
/// do special caterpillar update in the Link-Cut-Tree, i.e., given a node X, we
/// need to update the values of all of those nodes A such that A is the child
/// of one of the node in the path from Genesis to X.
///
/// In ConsensusGraphInner, every block corresponds to a ConsensusGraphNode and
/// each node has an internal index. This enables fast internal implementation
/// to use integer index instead of H256 block hashes.
pub struct ConsensusGraphInner {
    // This slab hold consensus graph node data and the array index is the
    // internal index.
    pub arena: Slab<ConsensusGraphNode>,
    // indices maps block hash to internal index.
    pub indices: HashMap<H256, usize>,
    // The current pivot chain indexes.
    pub pivot_chain: Vec<usize>,
    // The metadata associated with each pivot chain block
    pub pivot_chain_metadata: Vec<ConsensusGraphPivotData>,
    // The weight of all future blocks for each pivot block maintained in
    // a fenwick tree. See compute_future_weights() to see how it can be used
    // to compute future total weights.
    // pub pivot_future_weights: FenwickTree,
    // The set of *graph* tips in the TreeGraph.
    pub terminal_hashes: HashSet<H256>,
    genesis_block_index: usize,
    genesis_block_state_root: StateRoot,
    genesis_block_receipts_root: H256,
    // weight_tree maintains the subtree weight of each node in the TreeGraph
    weight_tree: MinLinkCutTree,
    inclusive_weight_tree: MinLinkCutTree,
    stable_weight_tree: MinLinkCutTree,
    // stable_tree maintains d * SubTW(B, x) + n * x.parent.weight + n *
    // PastW(x.parent)
    stable_tree: MinLinkCutTree,
    // adaptive_tree maintains d * SubStableTW(B, x) - n * SubTW(B, P(x))
    adaptive_tree: MinLinkCutTree,
    // inclusive_adaptive_tree maintains d * SubInclusiveTW(B, x) - n *
    // SubInclusiveTW(B, P(x))
    inclusive_adaptive_tree: MinLinkCutTree,
    pow_config: ProofOfWorkConfig,
    // It maintains the expected difficulty of the next local mined block.
    pub current_difficulty: U256,
    // data_man is the handle to access raw block data
    data_man: Arc<BlockDataManager>,
    // Optimistic execution is the feature to execute ahead of the deferred
    // execution boundary. The goal is to pipeline the transaction
    // execution and the block packaging and verification.
    // optimistic_executed_height is the number of step to go ahead
    optimistic_executed_height: Option<usize>,
    pub inner_conf: ConsensusInnerConfig,
    // The cache to store Anticone information of each node. This could be very
    // large so we periodically remove old ones in the cache.
    pub anticone_cache: AnticoneCache,
    pub sequence_number_of_block_entrance: u64,
}

pub struct ConsensusGraphNode {
    pub hash: H256,
    pub height: u64,
    pub is_heavy: bool,
    pub difficulty: U256,
    /// The total weight of its past set (exclude itself)
    pub past_weight: i128,
    /// The total weight of its past set in its own era
    pub past_era_weight: i128,
    pub pow_quality: U256,
    pub stable: bool,
    pub adaptive: bool,
    pub parent: usize,
    pub last_pivot_in_past: usize,
    pub children: Vec<usize>,
    pub referrers: Vec<usize>,
    pub referees: Vec<usize>,
    pub data: ConsensusGraphNodeData,
}

impl ConsensusGraphInner {
    pub fn with_genesis_block(
        pow_config: ProofOfWorkConfig, data_man: Arc<BlockDataManager>,
        inner_conf: ConsensusInnerConfig,
    ) -> Self
    {
        let mut inner = ConsensusGraphInner {
            arena: Slab::new(),
            indices: HashMap::new(),
            pivot_chain: Vec::new(),
            pivot_chain_metadata: Vec::new(),
            optimistic_executed_height: None,
            terminal_hashes: Default::default(),
            genesis_block_index: NULL,
            genesis_block_state_root: data_man
                .genesis_block()
                .block_header
                .deferred_state_root()
                .clone(),
            genesis_block_receipts_root: data_man
                .genesis_block()
                .block_header
                .deferred_receipts_root()
                .clone(),
            weight_tree: MinLinkCutTree::new(),
            inclusive_weight_tree: MinLinkCutTree::new(),
            stable_weight_tree: MinLinkCutTree::new(),
            stable_tree: MinLinkCutTree::new(),
            adaptive_tree: MinLinkCutTree::new(),
            inclusive_adaptive_tree: MinLinkCutTree::new(),
            pow_config,
            current_difficulty: pow_config.initial_difficulty.into(),
            data_man: data_man.clone(),
            inner_conf,
            anticone_cache: AnticoneCache::new(),
            sequence_number_of_block_entrance: 0,
        };

        // NOTE: Only genesis block will be first inserted into consensus graph
        // and then into synchronization graph. All the other blocks will be
        // inserted first into synchronization graph then consensus graph.
        // At current point, genesis block is not in synchronization graph,
        // so we cannot compute its past_weight from
        // sync_graph.total_weight_in_own_epoch().
        // For genesis block, its past_weight is simply zero.
        let (genesis_index, _) =
            inner.insert(data_man.genesis_block().as_ref());
        inner.genesis_block_index = genesis_index;
        inner.weight_tree.make_tree(inner.genesis_block_index);
        inner.weight_tree.path_apply(
            inner.genesis_block_index,
            into_i128(data_man.genesis_block().block_header.difficulty()),
        );
        inner
            .inclusive_weight_tree
            .make_tree(inner.genesis_block_index);
        inner.inclusive_weight_tree.path_apply(
            inner.genesis_block_index,
            into_i128(data_man.genesis_block().block_header.difficulty()),
        );
        inner
            .stable_weight_tree
            .make_tree(inner.genesis_block_index);
        inner.stable_weight_tree.path_apply(
            inner.genesis_block_index,
            into_i128(data_man.genesis_block().block_header.difficulty()),
        );
        inner.stable_tree.make_tree(inner.genesis_block_index);
        // The genesis node can be zero in stable_tree because it is never used!
        inner.stable_tree.set(inner.genesis_block_index, 0);
        inner.adaptive_tree.make_tree(inner.genesis_block_index);
        // The genesis node can be zero in adaptive_tree because it is never
        // used!
        inner.adaptive_tree.set(inner.genesis_block_index, 0);
        inner
            .inclusive_adaptive_tree
            .make_tree(inner.genesis_block_index);
        // The genesis node can be zero in adaptive_tree because it is never
        // used!
        inner
            .inclusive_adaptive_tree
            .set(inner.genesis_block_index, 0);
        inner.arena[inner.genesis_block_index].data.epoch_number = 0;
        inner.pivot_chain.push(inner.genesis_block_index);
        let mut last_pivot_in_past_blocks = HashSet::new();
        last_pivot_in_past_blocks.insert(inner.genesis_block_index);
        inner.pivot_chain_metadata.push(ConsensusGraphPivotData {
            last_pivot_in_past_blocks,
        });
        assert!(inner.genesis_block_receipts_root == KECCAK_EMPTY_LIST_RLP);

        inner.anticone_cache.update(0, &BitSet::new());
        inner
    }

    pub fn get_next_sequence_number(&mut self) -> u64 {
        let sn = self.sequence_number_of_block_entrance;
        self.sequence_number_of_block_entrance += 1;
        sn
    }

    pub fn is_heavier(a: (i128, &H256), b: (i128, &H256)) -> bool {
        (a.0 > b.0) || ((a.0 == b.0) && (*a.1 > *b.1))
    }

    fn get_era_height(
        &self, parent_height: u64, offset: usize,
    ) -> u64 {
        let era_height = if parent_height > offset as u64 {
            (parent_height - offset as u64) / ERA_EPOCH_COUNT as u64
                * ERA_EPOCH_COUNT as u64
        } else {
            0
        };
        era_height
    }

    fn get_era_block_with_parent(&self, parent: usize, offset: usize) -> usize {
        let height = self.arena[parent].height;
        let era_height = self.get_era_height(height, offset);
        self.weight_tree.ancestor_at(parent, era_height as usize)
    }

    pub fn get_optimistic_execution_task(
        &mut self, data_man: &BlockDataManager,
    ) -> Option<EpochExecutionTask> {
        if !self.inner_conf.enable_optimistic_execution {
            return None;
        }

        let opt_height = self.optimistic_executed_height?;
        let epoch_index = self.pivot_chain[opt_height];

        // `on_local_pivot` is set to `true` because when we later skip its
        // execution on pivot chain, we will not notify tx pool, so we
        // will also notify in advance.
        let execution_task = EpochExecutionTask::new(
            self.arena[epoch_index].hash,
            self.get_epoch_block_hashes(epoch_index),
            self.get_reward_execution_info(
                data_man,
                opt_height,
                &self.pivot_chain,
            ),
            true,
            false,
        );
        let next_opt_height = opt_height + 1;
        if next_opt_height >= self.pivot_chain.len() {
            self.optimistic_executed_height = None;
        } else {
            self.optimistic_executed_height = Some(next_opt_height);
        }
        Some(execution_task)
    }

    pub fn get_epoch_block_hashes(&self, epoch_index: usize) -> Vec<H256> {
        self.arena[epoch_index]
            .data
            .ordered_epoch_blocks
            .iter()
            .map(|idx| self.arena[*idx].hash)
            .collect()
    }

    pub fn check_mining_adaptive_block(
        &mut self, parent_index: usize, difficulty: U256,
    ) -> bool {
        let (_stable, adaptive) = self.adaptive_weight_impl(
            parent_index,
            &BitSet::new(),
            None,
            into_i128(&difficulty),
        );
        adaptive
    }

    fn compute_subtree_weights(
        &self, me: usize, anticone_barrier: &BitSet,
    ) -> (Vec<i128>, Vec<i128>, Vec<i128>) {
        let mut subtree_weight = Vec::new();
        let mut subtree_inclusive_weight = Vec::new();
        let mut subtree_stable_weight = Vec::new();
        let n = self.arena.len();
        subtree_weight.resize_with(n, Default::default);
        subtree_inclusive_weight.resize_with(n, Default::default);
        subtree_stable_weight.resize_with(n, Default::default);
        let mut stack = Vec::new();
        stack.push((0, self.genesis_block_index));
        while let Some((stage, index)) = stack.pop() {
            if stage == 0 {
                stack.push((1, index));
                for child in &self.arena[index].children {
                    if !anticone_barrier.contains(*child as u32) && *child != me
                    {
                        stack.push((0, *child));
                    }
                }
            } else {
                for child in &self.arena[index].children {
                    subtree_weight[index] += subtree_weight[*child];
                    subtree_inclusive_weight[index] +=
                        subtree_inclusive_weight[*child];
                    subtree_stable_weight[index] +=
                        subtree_stable_weight[*child];
                }
                let weight = self.block_weight(index, false);
                subtree_weight[index] += weight;
                subtree_inclusive_weight[index] +=
                    self.block_weight(index, true);
                if self.arena[index].stable {
                    subtree_stable_weight[index] += weight;
                }
            }
        }
        (
            subtree_weight,
            subtree_inclusive_weight,
            subtree_stable_weight,
        )
    }

    fn adaptive_weight_impl_brutal(
        &self, parent_0: usize, subtree_weight: &Vec<i128>,
        subtree_inclusive_weight: &Vec<i128>,
        subtree_stable_weight: &Vec<i128>, difficulty: i128,
    ) -> (bool, bool)
    {
        let mut parent = parent_0;
        let mut stable = true;

        let height = self.arena[parent].height;
        let era_height = self.get_era_height(height, 0);
        let two_era_height =
            self.get_era_height(height, ERA_EPOCH_COUNT);
        let era_genesis = self
            .inclusive_weight_tree
            .ancestor_at(parent, era_height as usize);

        let total_weight = subtree_weight[era_genesis];
        let adjusted_beta =
            (self.inner_conf.adaptive_weight_beta as i128) * difficulty;

        while self.arena[parent].height != era_height {
            let grandparent = self.arena[parent].parent;
            let w = total_weight
                - self.arena[grandparent].past_era_weight
                - self.block_weight(grandparent, false);
            if w > adjusted_beta {
                let a = subtree_weight[parent];
                if self.inner_conf.adaptive_weight_alpha_den as i128 * a
                    - self.inner_conf.adaptive_weight_alpha_num as i128 * w
                    < 0
                {
                    stable = false;
                    break;
                }
            }
            parent = grandparent;
        }
        let mut adaptive = false;
        if !stable {
            parent = parent_0;
            while self.arena[parent].height != era_height {
                let grandparent = self.arena[parent].parent;
                let w = subtree_weight[grandparent];
                if w > adjusted_beta {
                    let a = subtree_stable_weight[parent];
                    if self.inner_conf.adaptive_weight_alpha_den as i128 * a
                        - self.inner_conf.adaptive_weight_alpha_num as i128 * w
                        < 0
                    {
                        adaptive = true;
                        break;
                    }
                }
                parent = grandparent;
            }
        }
        if !adaptive {
            while self.arena[parent].height != two_era_height {
                let grandparent = self.arena[parent].parent;
                let w = subtree_inclusive_weight[grandparent];
                if w > adjusted_beta {
                    let a = subtree_inclusive_weight[parent];
                    if self.inner_conf.adaptive_weight_alpha_den as i128 * a
                        - self.inner_conf.adaptive_weight_alpha_num as i128 * w
                        < 0
                    {
                        adaptive = true;
                        break;
                    }
                }
            }
        }

        (stable, adaptive)
    }

    fn adaptive_weight_impl(
        &mut self, parent_0: usize, anticone_barrier: &BitSet,
        weight_tuple: Option<&(Vec<i128>, Vec<i128>, Vec<i128>)>,
        difficulty: i128,
    ) -> (bool, bool)
    {
        if let Some((
            subtree_weight,
            subtree_inclusive_weight,
            subtree_stable_weight,
        )) = weight_tuple
        {
            return self.adaptive_weight_impl_brutal(
                parent_0,
                subtree_weight,
                subtree_inclusive_weight,
                subtree_stable_weight,
                difficulty,
            );
        }
        let mut parent = parent_0;

        let mut weight_delta = HashMap::new();
        let mut inclusive_weight_delta = HashMap::new();
        let mut stable_weight_delta = HashMap::new();

        for index in anticone_barrier.iter() {
            weight_delta
                .insert(index as usize, self.weight_tree.get(index as usize));
            inclusive_weight_delta.insert(
                index as usize,
                self.inclusive_weight_tree.get(index as usize),
            );
            stable_weight_delta.insert(
                index as usize,
                self.stable_weight_tree.get(index as usize),
            );
        }

        for (index, delta) in &weight_delta {
            self.weight_tree.path_apply(*index, -delta);
            self.stable_tree.path_apply(
                *index,
                -delta * (self.inner_conf.adaptive_weight_alpha_den as i128),
            );
            let parent = self.arena[*index].parent;
            self.adaptive_tree.catepillar_apply(
                parent,
                delta * (self.inner_conf.adaptive_weight_alpha_num as i128),
            );
        }
        for (index, delta) in &inclusive_weight_delta {
            let parent = self.arena[*index].parent;
            self.inclusive_adaptive_tree.catepillar_apply(
                parent,
                delta * (self.inner_conf.adaptive_weight_alpha_num as i128),
            );
            self.inclusive_adaptive_tree.path_apply(
                *index,
                -delta * (self.inner_conf.adaptive_weight_alpha_den as i128),
            );
        }
        for (index, delta) in &stable_weight_delta {
            self.adaptive_tree.path_apply(
                *index,
                -delta * (self.inner_conf.adaptive_weight_alpha_den as i128),
            );
        }

        let era_height = self
            .get_era_height(self.arena[parent].height, 0);
        let two_era_height = self.get_era_height(
            self.arena[parent].height,
            ERA_EPOCH_COUNT,
        );
        let era_genesis = self
            .inclusive_weight_tree
            .ancestor_at(parent, era_height as usize);

        let total_weight = self.weight_tree.get(era_genesis);
        debug!("total_weight before insert: {}", total_weight);

        let adjusted_beta =
            (self.inner_conf.adaptive_weight_beta as i128) * difficulty;

        let mut high = self.arena[parent].height;
        let mut low = era_height + 1;
        // [low, high]
        let mut best = era_height;

        while low <= high {
            let mid = (low + high) / 2;
            let p = self.weight_tree.ancestor_at(parent, mid as usize);
            let gp = self.arena[p].parent;
            let w = total_weight
                - self.arena[gp].past_era_weight
                - self.block_weight(gp, false);
            if w > adjusted_beta {
                best = mid;
                low = mid + 1;
            } else {
                high = mid - 1;
            }
        }

        let stable = if best != era_height {
            parent = self.weight_tree.ancestor_at(parent, best as usize);

            let a = self.stable_tree.path_aggregate(parent);
            let b = total_weight
                * (self.inner_conf.adaptive_weight_alpha_num as i128);
            if a < b {
                debug!("block is unstable: {:?} < {:?}!", a, b);
            } else {
                debug!("block is stable: {:?} >= {:?}!", a, b);
            }
            !(a < b)
        } else {
            debug!(
                "block is stable: too close to genesis, adjusted beta {:?}",
                adjusted_beta
            );
            true
        };
        let mut adaptive = false;

        if !stable {
            parent = parent_0;

            let mut high = self.arena[parent].height;
            let mut low = era_height + 1;
            let mut best = era_height;

            while low <= high {
                let mid = (low + high) / 2;
                let p = self.weight_tree.ancestor_at(parent, mid as usize);
                let gp = self.arena[p].parent;
                let w = self.weight_tree.get(gp);
                if w > adjusted_beta {
                    best = mid;
                    low = mid + 1;
                } else {
                    high = mid - 1;
                }
            }

            if best != era_height {
                parent = self.weight_tree.ancestor_at(parent, best as usize);
                let min_agg = self.adaptive_tree.path_aggregate(parent);
                if min_agg < 0 {
                    debug!("block is adaptive (intra-era): {:?}", min_agg);
                    adaptive = true;
                }
            }
        }

        if !adaptive {
            let mut high = era_height;
            let mut low = two_era_height + 1;
            let mut best = two_era_height;

            while low <= high {
                let mid = (low + high) / 2;
                let p = self
                    .inclusive_weight_tree
                    .ancestor_at(parent, mid as usize);
                let gp = self.arena[p].parent;
                let w = self.inclusive_weight_tree.get(gp);
                if w > adjusted_beta {
                    best = mid;
                    low = mid + 1;
                } else {
                    high = mid - 1;
                }
            }

            if best != two_era_height {
                parent = self
                    .inclusive_weight_tree
                    .ancestor_at(parent, best as usize);
                let min_agg =
                    self.inclusive_adaptive_tree.path_aggregate(parent);
                if min_agg < 0 {
                    debug!("block is adaptive (inter-era): {:?}", min_agg);
                    adaptive = true;
                }
            }
        }

        for (index, delta) in &weight_delta {
            self.weight_tree.path_apply(*index, *delta);
            self.stable_tree.path_apply(
                *index,
                delta * (self.inner_conf.adaptive_weight_alpha_den as i128),
            );
            let parent = self.arena[*index].parent;
            self.adaptive_tree.catepillar_apply(
                parent,
                -delta * (self.inner_conf.adaptive_weight_alpha_num as i128),
            );
        }
        for (index, delta) in &inclusive_weight_delta {
            let parent = self.arena[*index].parent;
            self.inclusive_adaptive_tree.catepillar_apply(
                parent,
                -delta * (self.inner_conf.adaptive_weight_alpha_num as i128),
            );
            self.inclusive_adaptive_tree.path_apply(
                *index,
                delta * (self.inner_conf.adaptive_weight_alpha_den as i128),
            );
        }
        for (index, delta) in &stable_weight_delta {
            self.adaptive_tree.path_apply(
                *index,
                delta * (self.inner_conf.adaptive_weight_alpha_den as i128),
            );
        }

        (stable, adaptive)
    }

    pub fn adaptive_weight(
        &mut self, me: usize, anticone_barrier: &BitSet,
        weight_tuple: Option<&(Vec<i128>, Vec<i128>, Vec<i128>)>,
    ) -> (bool, bool)
    {
        let parent = self.arena[me].parent;
        assert!(parent != NULL);

        let difficulty = into_i128(&self.arena[me].difficulty);

        self.adaptive_weight_impl(
            parent,
            anticone_barrier,
            weight_tuple,
            difficulty,
        )
    }

    fn collect_blockset_in_own_view_of_epoch(&mut self, pivot: usize) {
        let mut queue = VecDeque::new();
        let mut visited = HashSet::new();
        for referee in &self.arena[pivot].referees {
            visited.insert(*referee);
            queue.push_back(*referee);
        }

        while let Some(index) = queue.pop_front() {
            let mut in_old_epoch = false;
            let mut parent = self.arena[pivot].parent;

            if self.arena[parent].height
                > self.arena[index].data.max_epoch_in_other_views
            {
                parent = self.weight_tree.ancestor_at(
                    parent,
                    self.arena[index].data.max_epoch_in_other_views as usize,
                );
            }

            loop {
                assert!(parent != NULL);

                if self.arena[parent].height
                    < self.arena[index].data.min_epoch_in_other_views
                    || (self.arena[index].data.sequence_number
                        > self.arena[parent].data.sequence_number)
                {
                    break;
                }

                if parent == index
                    || self.arena[parent]
                        .data
                        .blockset_in_own_view_of_epoch
                        .contains(&index)
                {
                    in_old_epoch = true;
                    break;
                }

                parent = self.arena[parent].parent;
            }

            if !in_old_epoch {
                let parent = self.arena[index].parent;
                if !visited.contains(&parent) {
                    visited.insert(parent);
                    queue.push_back(parent);
                }
                for referee in &self.arena[index].referees {
                    if !visited.contains(referee) {
                        visited.insert(*referee);
                        queue.push_back(*referee);
                    }
                }
                self.arena[index].data.min_epoch_in_other_views = min(
                    self.arena[index].data.min_epoch_in_other_views,
                    self.arena[pivot].height,
                );
                self.arena[index].data.max_epoch_in_other_views = max(
                    self.arena[index].data.max_epoch_in_other_views,
                    self.arena[pivot].height,
                );
                self.arena[pivot]
                    .data
                    .blockset_in_own_view_of_epoch
                    .insert(index);
            }
        }
        self.arena[pivot].data.ordered_epoch_blocks = self.topological_sort(
            &self.arena[pivot].data.blockset_in_own_view_of_epoch,
        );
        self.arena[pivot].data.ordered_epoch_blocks.push(pivot);
    }

    pub fn insert(&mut self, block: &Block) -> (usize, usize) {
        let hash = block.hash();

        let is_heavy = U512::from(block.block_header.pow_quality)
            >= U512::from(self.inner_conf.heavy_block_difficulty_ratio)
                * U512::from(block.block_header.difficulty());

        let parent = if *block.block_header.parent_hash() != H256::default() {
            self.indices
                .get(block.block_header.parent_hash())
                .cloned()
                .unwrap()
        } else {
            NULL
        };

        let referees: Vec<usize> = block
            .block_header
            .referee_hashes()
            .iter()
            .map(|hash| self.indices.get(hash).cloned().unwrap())
            .collect();
        for referee in &referees {
            self.terminal_hashes.remove(&self.arena[*referee].hash);
        }
        let my_height = block.block_header.height();
        let sn = self.get_next_sequence_number();
        let index = self.arena.insert(ConsensusGraphNode {
            hash,
            height: my_height,
            is_heavy,
            difficulty: *block.block_header.difficulty(),
            past_weight: 0,     // will be updated later below
            past_era_weight: 0, // will be updated later below
            pow_quality: block.block_header.pow_quality,
            stable: true,
            // Block header contains an adaptive field, we will verify with our
            // own computation
            adaptive: block.block_header.adaptive(),
            parent,
            last_pivot_in_past: 0,
            children: Vec::new(),
            referees,
            referrers: Vec::new(),
            data: ConsensusGraphNodeData::new(NULL, my_height, sn),
        });
        self.indices.insert(hash, index);

        if parent != NULL {
            self.terminal_hashes.remove(&self.arena[parent].hash);
            self.arena[parent].children.push(index);
        }
        self.terminal_hashes.insert(hash);
        let referees = self.arena[index].referees.clone();
        for referee in referees {
            self.arena[referee].referrers.push(index);
        }

        self.collect_blockset_in_own_view_of_epoch(index);

        if parent != NULL {
            let era_genesis = self.get_era_block_with_parent(parent, 0);

            let weight_in_my_epoch = self.total_weight_in_own_epoch(
                &self.arena[index].data.blockset_in_own_view_of_epoch,
                false,
                None,
            );
            let weight_era_in_my_epoch = self.total_weight_in_own_epoch(
                &self.arena[index].data.blockset_in_own_view_of_epoch,
                false,
                Some(era_genesis),
            );
            let past_weight = self.arena[parent].past_weight
                + self.block_weight(parent, false)
                + weight_in_my_epoch;
            let past_era_weight = if parent != era_genesis {
                self.arena[parent].past_era_weight
                    + self.block_weight(parent, false)
                    + weight_era_in_my_epoch
            } else {
                self.block_weight(parent, false) + weight_era_in_my_epoch
            };

            self.arena[index].past_weight = past_weight;
            self.arena[index].past_era_weight = past_era_weight;
        }

        debug!(
            "Block {} inserted into Consensus with index={} past_weight={}",
            hash, index, self.arena[index].past_weight
        );

        (index, self.indices.len())
    }

    fn check_correct_parent_brutal(
        &mut self, me: usize, subtree_weight: &Vec<i128>,
    ) -> bool {
        let mut valid = true;
        let parent = self.arena[me].parent;
        let parent_height = self.arena[parent].height;
        let era_height =
            self.get_era_height(parent_height, 0);

        // Check the pivot selection decision.
        for consensus_index_in_epoch in
            self.arena[me].data.blockset_in_own_view_of_epoch.iter()
        {
            if self.arena[*consensus_index_in_epoch].data.partial_invalid {
                continue;
            }

            let lca = self.weight_tree.lca(*consensus_index_in_epoch, parent);
            assert!(lca != *consensus_index_in_epoch);
            // If it is outside current era, we will skip!
            if self.arena[lca].height < era_height {
                continue;
            }
            if lca == parent {
                valid = false;
                break;
            }

            let fork = self.weight_tree.ancestor_at(
                *consensus_index_in_epoch,
                self.arena[lca].height as usize + 1,
            );
            let pivot = self
                .weight_tree
                .ancestor_at(parent, self.arena[lca].height as usize + 1);

            let fork_subtree_weight = subtree_weight[fork];
            let pivot_subtree_weight = subtree_weight[pivot];

            if ConsensusGraphInner::is_heavier(
                (fork_subtree_weight, &self.arena[fork].hash),
                (pivot_subtree_weight, &self.arena[pivot].hash),
            ) {
                valid = false;
                break;
            }
        }

        valid
    }

    fn check_correct_parent(
        &mut self, me: usize, anticone_barrier: &BitSet,
        weight_tuple: Option<&(Vec<i128>, Vec<i128>, Vec<i128>)>,
    ) -> bool
    {
        if let Some((subtree_weight, _, _)) = weight_tuple {
            return self.check_correct_parent_brutal(me, subtree_weight);
        }
        let mut valid = true;
        let parent = self.arena[me].parent;
        let parent_height = self.arena[parent].height;
        let era_height =
            self.get_era_height(parent_height, 0);

        let mut weight_delta = HashMap::new();

        for index in anticone_barrier {
            weight_delta
                .insert(index as usize, self.weight_tree.get(index as usize));
        }

        // Remove weight contribution of anticone
        for (index, delta) in &weight_delta {
            self.weight_tree.path_apply(*index, -delta);
        }

        // Check the pivot selection decision.
        for consensus_index_in_epoch in
            self.arena[me].data.blockset_in_own_view_of_epoch.iter()
        {
            if self.arena[*consensus_index_in_epoch].data.partial_invalid {
                continue;
            }

            let lca = self.weight_tree.lca(*consensus_index_in_epoch, parent);
            assert!(lca != *consensus_index_in_epoch);
            // If it is outside the era, we will skip!
            if self.arena[lca].height < era_height {
                continue;
            }
            if lca == parent {
                valid = false;
                break;
            }

            let fork = self.weight_tree.ancestor_at(
                *consensus_index_in_epoch,
                self.arena[lca].height as usize + 1,
            );
            let pivot = self
                .weight_tree
                .ancestor_at(parent, self.arena[lca].height as usize + 1);

            let fork_subtree_weight = self.weight_tree.get(fork);
            let pivot_subtree_weight = self.weight_tree.get(pivot);

            if ConsensusGraphInner::is_heavier(
                (fork_subtree_weight, &self.arena[fork].hash),
                (pivot_subtree_weight, &self.arena[pivot].hash),
            ) {
                valid = false;
                break;
            }
        }

        for (index, delta) in &weight_delta {
            self.weight_tree.path_apply(*index, *delta);
        }

        valid
    }

    fn compute_anticone_bruteforce(&self, me: usize) -> BitSet {
        let parent = self.arena[me].parent;
        let mut last_in_pivot = self.arena[parent].last_pivot_in_past;
        for referee in &self.arena[me].referees {
            last_in_pivot =
                max(last_in_pivot, self.arena[*referee].last_pivot_in_past);
        }
        let mut visited = BitSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(me);
        visited.add(me as u32);
        while let Some(index) = queue.pop_front() {
            let parent = self.arena[index].parent;
            if self.arena[parent].data.epoch_number > last_in_pivot
                && !visited.contains(parent as u32)
            {
                visited.add(parent as u32);
                queue.push_back(parent);
            }
            for referee in &self.arena[index].referees {
                if self.arena[*referee].data.epoch_number > last_in_pivot
                    && !visited.contains(*referee as u32)
                {
                    visited.add(*referee as u32);
                    queue.push_back(*referee);
                }
            }
        }
        let mut anticone = BitSet::new();
        for i in 0..self.arena.len() {
            if self.arena[i].data.epoch_number > last_in_pivot
                && !visited.contains(i as u32)
            {
                anticone.add(i as u32);
            }
        }
        anticone
    }

    pub fn compute_anticone(&mut self, me: usize) -> BitSet {
        let parent = self.arena[me].parent;
        debug_assert!(parent != NULL);
        debug_assert!(self.arena[me].children.is_empty());
        debug_assert!(self.arena[me].referrers.is_empty());

        // If we do not have the anticone of its parent, we compute it with
        // brute force!
        let parent_anticone_opt = self.anticone_cache.get(parent);
        let mut anticone;
        if parent_anticone_opt.is_none() {
            anticone = self.compute_anticone_bruteforce(me);
        } else {
            // Compute future set of parent
            let mut parent_futures = BitSet::new();
            let mut queue: VecDeque<usize> = VecDeque::new();
            let mut visited = BitSet::new();
            queue.push_back(parent);
            while let Some(index) = queue.pop_front() {
                if visited.contains(index as u32) {
                    continue;
                }
                if index != parent && index != me {
                    parent_futures.add(index as u32);
                }

                visited.add(index as u32);
                for child in &self.arena[index].children {
                    queue.push_back(*child);
                }
                for referrer in &self.arena[index].referrers {
                    queue.push_back(*referrer);
                }
            }

            anticone = {
                let parent_anticone = parent_anticone_opt.unwrap();
                let mut my_past = BitSet::new();
                debug_assert!(queue.is_empty());
                queue.push_back(me);
                while let Some(index) = queue.pop_front() {
                    if my_past.contains(index as u32) {
                        continue;
                    }

                    debug_assert!(index != parent);
                    if index != me {
                        my_past.add(index as u32);
                    }

                    let idx_parent = self.arena[index].parent;
                    debug_assert!(idx_parent != NULL);
                    if parent_anticone.contains(&idx_parent)
                        || parent_futures.contains(idx_parent as u32)
                    {
                        queue.push_back(idx_parent);
                    }

                    for referee in &self.arena[index].referees {
                        if parent_anticone.contains(referee)
                            || parent_futures.contains(*referee as u32)
                        {
                            queue.push_back(*referee);
                        }
                    }
                }
                for index in parent_anticone {
                    parent_futures.add(*index as u32);
                }
                for index in my_past.drain() {
                    parent_futures.remove(index);
                }
                parent_futures
            };
        }

        self.anticone_cache.update(me, &anticone);

        let mut anticone_barrier = BitSet::new();
        for index in anticone.clone().iter() {
            let parent = self.arena[index as usize].parent as u32;
            if !anticone.contains(parent) {
                anticone_barrier.add(index);
            }
        }

        debug!(
            "Block {} anticone size {}",
            self.arena[me].hash,
            anticone.len()
        );

        anticone_barrier
    }

    fn topological_sort(&self, index_set: &HashSet<usize>) -> Vec<usize> {
        let mut num_incoming_edges = HashMap::new();

        for me in index_set {
            num_incoming_edges.entry(*me).or_insert(0);
            let parent = self.arena[*me].parent;
            if index_set.contains(&parent) {
                *num_incoming_edges.entry(parent).or_insert(0) += 1;
            }
            for referee in &self.arena[*me].referees {
                if index_set.contains(referee) {
                    *num_incoming_edges.entry(*referee).or_insert(0) += 1;
                }
            }
        }

        let mut candidates = HashSet::new();
        let mut reversed_indices = Vec::new();

        for me in index_set {
            if num_incoming_edges[me] == 0 {
                candidates.insert(*me);
            }
        }
        while !candidates.is_empty() {
            let me = candidates
                .iter()
                .max_by_key(|index| self.arena[**index].hash)
                .cloned()
                .unwrap();
            candidates.remove(&me);
            reversed_indices.push(me);

            let parent = self.arena[me].parent;
            if index_set.contains(&parent) {
                num_incoming_edges.entry(parent).and_modify(|e| *e -= 1);
                if num_incoming_edges[&parent] == 0 {
                    candidates.insert(parent);
                }
            }
            for referee in &self.arena[me].referees {
                if index_set.contains(referee) {
                    num_incoming_edges.entry(*referee).and_modify(|e| *e -= 1);
                    if num_incoming_edges[referee] == 0 {
                        candidates.insert(*referee);
                    }
                }
            }
        }
        reversed_indices.reverse();
        reversed_indices
    }

    /// Return the consensus graph indexes of the pivot block where the rewards
    /// of its epoch should be computed The rewards are needed to compute
    /// the state of the epoch at height `state_at` of `chain`
    fn get_pivot_reward_index(
        &self, state_at: usize, chain: &Vec<usize>,
    ) -> Option<(usize, usize)> {
        if state_at > REWARD_EPOCH_COUNT as usize {
            let epoch_num = state_at - REWARD_EPOCH_COUNT as usize;
            let anticone_penalty_cutoff_epoch_index =
                epoch_num + ANTICONE_PENALTY_UPPER_EPOCH_COUNT as usize;
            let pivot_index = chain[epoch_num];
            debug_assert!(epoch_num == self.arena[pivot_index].height as usize);
            debug_assert!(
                epoch_num == self.arena[pivot_index].data.epoch_number
            );
            Some((pivot_index, chain[anticone_penalty_cutoff_epoch_index]))
        } else {
            None
        }
    }

    pub fn get_epoch_blocks(
        &self, data_man: &BlockDataManager, epoch_index: usize,
    ) -> Vec<Arc<Block>> {
        let mut epoch_blocks = Vec::new();
        for idx in &self.arena[epoch_index].data.ordered_epoch_blocks {
            let block = data_man
                .block_by_hash(&self.arena[*idx].hash, false)
                .expect("Exist");
            epoch_blocks.push(block);
        }
        epoch_blocks
    }

    fn recompute_anticone_weight(
        &self, me: usize, pivot_block_index: usize,
    ) -> i128 {
        // We need to compute the future size of me under the view of epoch
        // height pivot_index
        let mut visited = BitSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(pivot_block_index);
        visited.add(pivot_block_index as u32);
        let last_pivot = self.arena[me].last_pivot_in_past;
        while let Some(index) = queue.pop_front() {
            let parent = self.arena[index].parent;
            if self.arena[parent].data.epoch_number > last_pivot
                && !visited.contains(parent as u32)
            {
                queue.push_back(parent);
                visited.add(parent as u32);
            }
            for referee in &self.arena[index].referees {
                if self.arena[*referee].data.epoch_number > last_pivot
                    && !visited.contains(*referee as u32)
                {
                    queue.push_back(*referee);
                    visited.add(*referee as u32);
                }
            }
        }
        queue.push_back(me);
        let mut visited2 = BitSet::new();
        visited2.add(me as u32);
        while let Some(index) = queue.pop_front() {
            for child in &self.arena[index].children {
                if visited.contains(*child as u32)
                    && !visited2.contains(*child as u32)
                {
                    queue.push_back(*child);
                    visited2.add(*child as u32);
                }
            }
            for referrer in &self.arena[index].referrers {
                if visited.contains(*referrer as u32)
                    && !visited2.contains(*referrer as u32)
                {
                    queue.push_back(*referrer);
                    visited2.add(*referrer as u32);
                }
            }
        }
        let mut total_weight = self.arena[pivot_block_index].past_weight
            - self.arena[me].past_weight
            + self.block_weight(pivot_block_index, false);
        for index in visited2.iter() {
            total_weight -= self.block_weight(index as usize, false);
        }
        total_weight
    }

    // TODO: consider moving the logic to background when consensus locks are
    // broken down.
    fn get_reward_execution_info_from_index(
        &self, data_man: &BlockDataManager,
        reward_index: Option<(usize, usize)>,
    ) -> Option<RewardExecutionInfo>
    {
        reward_index.map(
            |(pivot_index, anticone_penalty_cutoff_epoch_index)| {
                let epoch_blocks = self.get_epoch_blocks(data_man, pivot_index);

                let mut epoch_block_anticone_overlimited =
                    Vec::with_capacity(epoch_blocks.len());
                let mut epoch_block_anticone_difficulties =
                    Vec::with_capacity(epoch_blocks.len());

                let epoch_difficulty = self.arena[pivot_index].difficulty;
                let anticone_cutoff_epoch_anticone_set_opt = self
                    .anticone_cache
                    .get(anticone_penalty_cutoff_epoch_index);
                for index in &self.arena[pivot_index].data.ordered_epoch_blocks
                {
                    let block_consensus_node = &self.arena[*index];

                    let mut anticone_overlimited =
                        block_consensus_node.data.partial_invalid;
                    // If a block is partial_invalid, it won't have reward and
                    // anticone_difficulty will not be used, so it's okay to set
                    // it to 0.
                    let mut anticone_difficulty: U512 = 0.into();
                    if !anticone_overlimited {
                        let block_consensus_node_anticone_opt =
                            self.anticone_cache.get(*index);
                        if block_consensus_node_anticone_opt.is_none()
                            || anticone_cutoff_epoch_anticone_set_opt.is_none()
                        {
                            anticone_difficulty = U512::from(into_u256(
                                self.recompute_anticone_weight(
                                    *index,
                                    anticone_penalty_cutoff_epoch_index,
                                ),
                            ));
                        } else {
                            let anticone_set =
                                block_consensus_node_anticone_opt
                                    .unwrap()
                                    .difference(
                                        anticone_cutoff_epoch_anticone_set_opt
                                            .unwrap(),
                                    )
                                    .cloned()
                                    .collect::<HashSet<_>>();
                            for a_index in anticone_set {
                                // TODO: Maybe consider to use base difficulty
                                // Check with the spec!
                                anticone_difficulty += U512::from(into_u256(
                                    self.block_weight(a_index, false),
                                ));
                            }
                        };

                        // TODO: check the clear definition of anticone penalty,
                        // normally and around the time of difficulty
                        // adjustment.
                        // LINT.IfChange(ANTICONE_PENALTY_1)
                        if anticone_difficulty / U512::from(epoch_difficulty)
                            >= U512::from(ANTICONE_PENALTY_RATIO)
                        {
                            anticone_overlimited = true;
                        }
                        // LINT.ThenChange(consensus/consensus_executor.
                        // rs#ANTICONE_PENALTY_2)
                    }
                    epoch_block_anticone_overlimited.push(anticone_overlimited);
                    epoch_block_anticone_difficulties.push(anticone_difficulty);
                }
                RewardExecutionInfo {
                    epoch_blocks,
                    epoch_block_anticone_overlimited,
                    epoch_block_anticone_difficulties,
                }
            },
        )
    }

    fn get_reward_execution_info(
        &self, data_man: &BlockDataManager, state_at: usize, chain: &Vec<usize>,
    ) -> Option<RewardExecutionInfo> {
        self.get_reward_execution_info_from_index(
            data_man,
            self.get_pivot_reward_index(state_at, chain),
        )
    }

    pub fn expected_difficulty(&self, parent_hash: &H256) -> U256 {
        let parent_index = *self.indices.get(parent_hash).unwrap();
        let parent_epoch = self.arena[parent_index].height;
        if parent_epoch < self.pow_config.difficulty_adjustment_epoch_period {
            // Use initial difficulty for early epochs
            self.pow_config.initial_difficulty.into()
        } else {
            let last_period_upper = (parent_epoch
                / self.pow_config.difficulty_adjustment_epoch_period)
                * self.pow_config.difficulty_adjustment_epoch_period;
            if last_period_upper != parent_epoch {
                self.arena[parent_index].difficulty
            } else {
                let mut cur = parent_index;
                while self.arena[cur].height > last_period_upper {
                    cur = self.arena[cur].parent;
                }
                target_difficulty(
                    &self.data_man,
                    &self.pow_config,
                    &self.arena[cur].hash,
                    |h| {
                        let index = self.indices.get(h).unwrap();
                        self.arena[*index]
                            .data
                            .blockset_in_own_view_of_epoch
                            .len()
                    },
                )
            }
        }
    }

    pub fn adjust_difficulty(&mut self, new_best_index: usize) {
        let new_best_hash = self.arena[new_best_index].hash.clone();
        let new_best_difficulty = self.arena[new_best_index].difficulty;
        let old_best_index = *self.pivot_chain.last().expect("not empty");
        if old_best_index == self.arena[new_best_index].parent {
            // Pivot chain prolonged
            assert!(self.current_difficulty == new_best_difficulty);
        }

        let epoch = self.arena[new_best_index].height;
        if epoch == 0 {
            // This may happen since the block at height 1 may have wrong
            // state root and do not update the pivot chain.
            self.current_difficulty = self.pow_config.initial_difficulty.into();
        } else if epoch
            == (epoch / self.pow_config.difficulty_adjustment_epoch_period)
                * self.pow_config.difficulty_adjustment_epoch_period
        {
            self.current_difficulty = target_difficulty(
                &self.data_man,
                &self.pow_config,
                &new_best_hash,
                |h| {
                    let index = self.indices.get(h).unwrap();
                    self.arena[*index].data.blockset_in_own_view_of_epoch.len()
                },
            );
        } else {
            self.current_difficulty = new_best_difficulty;
        }
    }

    pub fn best_block_hash(&self) -> H256 {
        self.arena[*self.pivot_chain.last().unwrap()].hash
    }

    pub fn best_state_epoch_number(&self) -> usize {
        let pivot_len = self.pivot_chain.len();
        if pivot_len < DEFERRED_STATE_EPOCH_COUNT as usize {
            0
        } else {
            pivot_len - DEFERRED_STATE_EPOCH_COUNT as usize
        }
    }

    pub fn best_state_index(&self) -> usize {
        self.pivot_chain[self.best_state_epoch_number()]
    }

    pub fn best_state_block_hash(&self) -> H256 {
        self.arena[self.best_state_index()].hash
    }

    /// Return None if the best state is not executed or the db returned error
    // TODO check if we can ignore the db error
    pub fn try_get_best_state<'a>(
        &self, data_man: &'a BlockDataManager,
    ) -> Option<State<'a>> {
        let best_state_hash = self.best_state_block_hash();
        if let Ok(state) = data_man
            .storage_manager
            .get_state_no_commit(best_state_hash)
        {
            state.map(|db| {
                State::new(StateDb::new(db), 0.into(), Default::default())
            })
        } else {
            warn!("try_get_best_state: Error for hash {}", best_state_hash);
            None
        }
    }

    pub fn best_epoch_number(&self) -> usize { self.pivot_chain.len() - 1 }

    pub fn get_height_from_epoch_number(
        &self, epoch_number: EpochNumber,
    ) -> Result<usize, String> {
        Ok(match epoch_number {
            EpochNumber::Earliest => 0,
            EpochNumber::LatestMined => self.best_epoch_number(),
            EpochNumber::LatestState => self.best_state_epoch_number(),
            EpochNumber::Number(num) => {
                let epoch_num: usize = num.as_usize();
                if epoch_num > self.best_epoch_number() {
                    return Err("Invalid params: expected a numbers with less than largest epoch number.".to_owned());
                }
                epoch_num
            }
        })
    }

    pub fn get_index_from_epoch_number(
        &self, epoch_number: EpochNumber,
    ) -> Result<usize, String> {
        self.get_height_from_epoch_number(epoch_number)
            .and_then(|height| Ok(self.pivot_chain[height]))
    }

    pub fn get_hash_from_epoch_number(
        &self, epoch_number: EpochNumber,
    ) -> Result<H256, String> {
        self.get_index_from_epoch_number(epoch_number)
            .and_then(|index| Ok(self.arena[index].hash))
    }

    pub fn block_hashes_by_epoch(
        &self, epoch_number: EpochNumber,
    ) -> Result<Vec<H256>, String> {
        debug!(
            "block_hashes_by_epoch epoch_number={:?} pivot_chain.len={:?}",
            epoch_number,
            self.pivot_chain.len()
        );
        self.get_index_from_epoch_number(epoch_number)
            .and_then(|index| {
                Ok(self.arena[index]
                    .data
                    .ordered_epoch_blocks
                    .iter()
                    .map(|index| self.arena[*index].hash)
                    .collect())
            })
    }

    pub fn epoch_hash(&self, epoch_number: usize) -> Option<H256> {
        self.pivot_chain
            .get(epoch_number)
            .map(|idx| self.arena[*idx].hash)
    }

    pub fn get_epoch_hash_for_block(&self, hash: &H256) -> Option<H256> {
        self.indices.get(hash).and_then(|block_index| {
            let epoch_number = self.arena[*block_index].data.epoch_number;
            self.epoch_hash(epoch_number)
        })
    }

    pub fn get_balance(
        &self, address: H160, epoch_number: EpochNumber,
    ) -> Result<U256, String> {
        let hash = self.get_hash_from_epoch_number(epoch_number.clone())?;
        let maybe_state = self
            .data_man
            .storage_manager
            .get_state_no_commit(hash)
            .map_err(|e| format!("Error to get state, err={:?}", e))?;
        if let Some(state) = maybe_state {
            let state_db = StateDb::new(state);
            Ok(
                if let Ok(maybe_acc) = state_db.get_account(&address, false) {
                    maybe_acc.map_or(U256::zero(), |acc| acc.balance).into()
                } else {
                    0.into()
                },
            )
        } else {
            Err(format!(
                "State for epoch (number={:?} hash={:?}) does not exist",
                epoch_number, hash
            )
            .into())
        }
    }

    pub fn terminal_hashes(&self) -> Vec<H256> {
        self.terminal_hashes
            .iter()
            .map(|hash| hash.clone())
            .collect()
    }

    pub fn get_block_epoch_number(&self, hash: &H256) -> Option<usize> {
        if let Some(idx) = self.indices.get(hash) {
            Some(self.arena[*idx].data.epoch_number)
        } else {
            None
        }
    }

    pub fn all_blocks_with_topo_order(&self) -> Vec<H256> {
        let epoch_number = self.best_epoch_number();
        let mut current_number = 0;
        let mut hashes = Vec::new();
        while current_number <= epoch_number {
            let epoch_hashes = self
                .block_hashes_by_epoch(EpochNumber::Number(
                    current_number.into(),
                ))
                .unwrap();
            for hash in epoch_hashes {
                hashes.push(hash);
            }
            current_number += 1;
        }
        hashes
    }

    fn validate_stated_epoch(
        &self, epoch_number: &EpochNumber,
    ) -> Result<(), String> {
        match epoch_number {
            EpochNumber::LatestMined => {
                return Err("Latest mined epoch is not executed".into());
            }
            EpochNumber::Number(num) => {
                let latest_state_epoch = self.best_state_epoch_number();
                if num.as_usize() > latest_state_epoch {
                    return Err(format!("Specified epoch {} is not executed, the latest state epoch is {}", num, latest_state_epoch));
                }
            }
            _ => {}
        }

        Ok(())
    }

    pub fn block_receipts_by_hash(
        &self, hash: &H256, update_cache: bool,
    ) -> Option<Arc<Vec<Receipt>>> {
        self.get_epoch_hash_for_block(hash).and_then(|epoch| {
            trace!("Block {} is in epoch {}", hash, epoch);
            self.data_man
                .block_results_by_hash_with_epoch(hash, &epoch, update_cache)
                .map(|r| r.receipts)
        })
    }

    pub fn is_stable(&self, block_hash: &H256) -> Option<bool> {
        self.indices
            .get(block_hash)
            .and_then(|block_index| Some(self.arena[*block_index].stable))
    }

    pub fn is_adaptive(&self, block_hash: &H256) -> Option<bool> {
        self.indices
            .get(block_hash)
            .and_then(|block_index| Some(self.arena[*block_index].adaptive))
    }

    pub fn is_partial_invalid(&self, block_hash: &H256) -> Option<bool> {
        self.indices.get(block_hash).and_then(|block_index| {
            Some(self.arena[*block_index].data.partial_invalid)
        })
    }

    pub fn get_transaction_receipt_with_address(
        &self, tx_hash: &H256,
    ) -> Option<(Receipt, TransactionAddress)> {
        trace!("Get receipt with tx_hash {}", tx_hash);
        let address =
            self.data_man.transaction_address_by_hash(tx_hash, false)?;
        // receipts should never be None if address is not None because
        let receipts =
            self.block_receipts_by_hash(&address.block_hash, false)?;
        Some((
            receipts
                .get(address.index)
                .expect("Error: can't get receipt by tx_address ")
                .clone(),
            address,
        ))
    }

    pub fn transaction_count(
        &self, address: H160, epoch_number: EpochNumber,
    ) -> Result<U256, String> {
        self.validate_stated_epoch(&epoch_number)?;

        let hash = self.get_hash_from_epoch_number(epoch_number)?;
        let state_db = StateDb::new(
            self.data_man
                .storage_manager
                .get_state_no_commit(hash)
                .unwrap()
                .unwrap(),
        );
        let state = State::new(state_db, 0.into(), Default::default());
        state
            .nonce(&address)
            .map_err(|err| format!("Get transaction count error: {:?}", err))
    }

    pub fn get_balance_validated(
        &self, address: H160, epoch_number: EpochNumber,
    ) -> Result<U256, String> {
        self.validate_stated_epoch(&epoch_number)?;
        self.get_balance(address, epoch_number)
    }

    pub fn check_block_pivot_assumption(
        &self, pivot_hash: &H256, epoch: usize,
    ) -> Result<(), String> {
        let last_number = self
            .get_height_from_epoch_number(EpochNumber::LatestMined)
            .unwrap();
        let hash =
            self.get_hash_from_epoch_number(EpochNumber::Number(epoch.into()))?;
        if epoch > last_number || hash != *pivot_hash {
            return Err("Error: pivot chain assumption failed".to_owned());
        }
        Ok(())
    }

    pub fn persist_terminals(&self) {
        let mut terminals = Vec::with_capacity(self.terminal_hashes.len());
        for h in &self.terminal_hashes {
            terminals.push(h);
        }
        let mut rlp_stream = RlpStream::new();
        rlp_stream.begin_list(terminals.len());
        for hash in terminals {
            rlp_stream.append(hash);
        }
        let mut dbops = self.data_man.db.key_value().transaction();
        dbops.put(COL_MISC, b"terminals", &rlp_stream.drain());
        self.data_man.db.key_value().write(dbops).expect("db error");
    }

    /// Compute the block weight following the GHAST algorithm:
    /// For partially invalid block, the weight is always 0
    /// If a block is not adaptive, the weight is its difficulty
    /// If a block is adaptive, then for the heavy blocks, it equals to
    /// the heavy block ratio. Otherwise, it is zero.
    fn block_weight(&self, me: usize, inclusive: bool) -> i128 {
        if self.arena[me].data.partial_invalid && !inclusive {
            return 0 as i128;
        }
        let is_heavy = self.arena[me].is_heavy;
        let is_adaptive = self.arena[me].adaptive;
        if is_adaptive {
            if is_heavy {
                self.inner_conf.heavy_block_difficulty_ratio as i128
                    * into_i128(&self.arena[me].difficulty)
            } else {
                0 as i128
            }
        } else {
            into_i128(&self.arena[me].difficulty)
        }
    }

    /// Compute the total weight in the epoch represented by the block of
    /// my_hash.
    pub fn total_weight_in_own_epoch(
        &self, blockset_in_own_epoch: &HashSet<usize>, inclusive: bool,
        genesis_opt: Option<usize>,
    ) -> i128
    {
        let gen_index = if let Some(x) = genesis_opt {
            x
        } else {
            self.genesis_block_index
        };
        let gen_height = self.arena[gen_index].height;
        let mut total_weight = 0 as i128;
        for index in blockset_in_own_epoch.iter() {
            if gen_index != self.genesis_block_index {
                let height = self.arena[*index].height;
                if height < gen_height {
                    continue;
                }
                let era_index =
                    self.weight_tree.ancestor_at(*index, gen_height as usize);
                if gen_index != era_index {
                    continue;
                }
            }
            total_weight += self.block_weight(*index, inclusive);
        }
        total_weight
    }

    /// Binary search to find the starting point so we can execute to the end of
    /// the chain.
    /// Return the first index that is not executed,
    /// or return `chain.len()` if they are all executed (impossible for now).
    ///
    /// NOTE: If a state for an block exists, all the blocks on its pivot chain
    /// must have been executed and state committed. The receipts for these
    /// past blocks may not exist because the receipts on forks will be
    /// garbage-collected, but when we need them, we will recompute these
    /// missing receipts in `process_rewards_and_fees`. This 'recompute' is safe
    /// because the parent state exists. Thus, it's okay that here we do not
    /// check existence of the receipts that will be needed for reward
    /// computation during epoch execution.
    fn find_start_index(&self, chain: &Vec<usize>) -> usize {
        let mut base = 0;
        let mut size = chain.len();
        while size > 1 {
            let half = size / 2;
            let mid = base + half;
            let epoch_hash = self.arena[chain[mid]].hash;
            base = if self.data_man.epoch_executed(&epoch_hash) {
                mid
            } else {
                base
            };
            size -= half;
        }
        let epoch_hash = self.arena[chain[base]].hash;
        if self.data_man.epoch_executed(&epoch_hash) {
            base + 1
        } else {
            base
        }
    }
}

pub struct FinalityManager {
    pub lowest_epoch_num: usize,
    pub risks_less_than: VecDeque<f64>,
}

pub struct TotalWeightInPast {
    pub old: U256,
    pub cur: U256,
    pub delta: U256,
}

/// ConsensusGraph is a layer on top of SynchronizationGraph. A SyncGraph
/// collect all blocks that the client has received so far, but a block can only
/// be delivered to the ConsensusGraph if 1) the whole block content is
/// available and 2) all of its past blocks are also in the ConsensusGraph.
///
/// ConsensusGraph maintains the TreeGraph structure of the client and
/// implements *GHAST*/*Conflux* algorithm to determine the block total order.
/// It dispatches transactions in epochs to ConsensusExecutor to process. To
/// avoid executing too many execution reroll caused by transaction order
/// oscillation. It defers the transaction execution for a few epochs.
pub struct ConsensusGraph {
    pub conf: ConsensusConfig,
    pub inner: Arc<RwLock<ConsensusGraphInner>>,
    pub txpool: SharedTransactionPool,
    pub data_man: Arc<BlockDataManager>,
    pub invalid_blocks: RwLock<HashSet<H256>>,
    executor: Arc<ConsensusExecutor>,
    pub statistics: SharedStatistics,
    finality_manager: RwLock<FinalityManager>,
    pub total_weight_in_past_2d: RwLock<TotalWeightInPast>,
}

pub type SharedConsensusGraph = Arc<ConsensusGraph>;

impl ConfirmationTrait for ConsensusGraph {
    fn confirmation_risk_by_hash(&self, hash: H256) -> Option<f64> {
        let inner = self.inner.read();
        let index = *inner.indices.get(&hash)?;
        let epoch_num = inner.arena[index].data.epoch_number;
        if epoch_num == NULL {
            return None;
        }

        if epoch_num == 0 {
            return Some(0.0);
        }

        let finality = self.finality_manager.read();

        if epoch_num < finality.lowest_epoch_num {
            return Some(MIN_MAINTAINED_RISK);
        }

        let idx = epoch_num - finality.lowest_epoch_num;
        if idx < finality.risks_less_than.len() {
            let mut max_risk = 0.0;
            for i in 0..idx + 1 {
                let risk = *finality.risks_less_than.get(i).unwrap();
                if max_risk < risk {
                    max_risk = risk;
                }
            }
            Some(max_risk)
        } else {
            None
        }
    }
}

impl ConsensusGraph {
    /// Build the ConsensusGraph with a genesis block and various other
    /// components The execution will be skipped if bench_mode sets to true.
    pub fn with_genesis_block(
        conf: ConsensusConfig, vm: VmFactory, txpool: SharedTransactionPool,
        statistics: SharedStatistics, data_man: Arc<BlockDataManager>,
        pow_config: ProofOfWorkConfig,
    ) -> Self
    {
        let inner =
            Arc::new(RwLock::new(ConsensusGraphInner::with_genesis_block(
                pow_config,
                data_man.clone(),
                conf.inner_conf.clone(),
            )));
        let executor = Arc::new(ConsensusExecutor::start(
            txpool.clone(),
            data_man.clone(),
            vm,
            inner.clone(),
            conf.bench_mode,
        ));

        ConsensusGraph {
            conf,
            inner,
            txpool,
            data_man: data_man.clone(),
            invalid_blocks: RwLock::new(HashSet::new()),
            executor,
            statistics,
            finality_manager: RwLock::new(FinalityManager {
                lowest_epoch_num: 0,
                risks_less_than: VecDeque::new(),
            }),
            total_weight_in_past_2d: RwLock::new(TotalWeightInPast {
                old: U256::zero(),
                cur: U256::zero(),
                delta: U256::zero(),
            }),
        }
    }

    pub fn expected_difficulty(&self, parent_hash: &H256) -> U256 {
        let inner = self.inner.read();
        inner.expected_difficulty(parent_hash)
    }

    pub fn get_to_propagate_trans(
        &self,
    ) -> HashMap<H256, Arc<SignedTransaction>> {
        self.txpool.get_to_propagate_trans()
    }

    pub fn set_to_propagate_trans(
        &self, transactions: HashMap<H256, Arc<SignedTransaction>>,
    ) {
        self.txpool.set_to_propagate_trans(transactions);
    }

    pub fn update_total_weight_in_past(&self) {
        let mut total_weight = self.total_weight_in_past_2d.write();
        total_weight.delta = total_weight.cur - total_weight.old;
        total_weight.old = total_weight.cur;
    }

    pub fn aggregate_total_weight_in_past(&self, weight: i128) {
        let mut total_weight = self.total_weight_in_past_2d.write();
        total_weight.cur += into_u256(weight);
    }

    pub fn get_total_weight_in_past(&self) -> i128 {
        let total_weight = self.total_weight_in_past_2d.read();
        into_i128(&total_weight.delta)
    }

    fn confirmation_risk(
        &self, inner: &mut ConsensusGraphInner, w_0: i128, w_4: i128,
        epoch_num: usize,
    ) -> f64
    {
        // Compute w_1
        let idx = inner.pivot_chain[epoch_num];
        let w_1 = inner.block_weight(idx, false);

        // Compute w_2
        let parent = inner.arena[idx].parent;
        assert!(parent != NULL);
        let mut max_weight = 0;
        for child in inner.arena[parent].children.iter() {
            if *child == idx || inner.arena[*child].data.partial_invalid {
                continue;
            }

            let child_weight = inner.block_weight(*child, false);
            if child_weight > max_weight {
                max_weight = child_weight;
            }
        }
        let w_2 = max_weight;

        // Compute w_3
        let w_3 = inner.arena[idx].past_weight;

        // Compute d
        let d = into_i128(&inner.current_difficulty);

        // Compute n
        let w_2_4 = w_2 + w_4;
        let n = if w_1 >= w_2_4 { w_1 - w_2_4 } else { 0 };

        let n = (n / d) + 1;

        // Compute m
        let m = if w_0 >= w_3 { w_0 - w_3 } else { 0 };

        let m = m / d;

        // Compute risk
        let m_2 = 2i128 * m;
        let e_1 = m_2 / 5i128;
        let e_2 = m_2 / 7i128;
        let n_min_1 = e_1 + 13i128;
        let n_min_2 = e_2 + 36i128;
        let n_min = if n_min_1 < n_min_2 { n_min_1 } else { n_min_2 };

        let mut risk = 0.9;
        if n <= n_min {
            return risk;
        }

        risk = 0.0001;

        let n_min_1 = e_1 + 19i128;
        let n_min_2 = e_2 + 57i128;
        let n_min = if n_min_1 < n_min_2 { n_min_1 } else { n_min_2 };

        if n <= n_min {
            return risk;
        }

        risk = 0.000001;
        risk
    }

    fn update_confirmation_risks(
        &self, inner: &mut ConsensusGraphInner, w_4: i128,
    ) {
        if inner.pivot_chain.len() > DEFERRED_STATE_EPOCH_COUNT as usize {
            let w_0 = inner.weight_tree.get(inner.genesis_block_index);
            let mut risks = VecDeque::new();
            let mut epoch_num =
                inner.pivot_chain.len() - DEFERRED_STATE_EPOCH_COUNT as usize;
            let mut count = 0;
            while epoch_num > 0 && count < MAX_NUM_MAINTAINED_RISK {
                let risk = self.confirmation_risk(inner, w_0, w_4, epoch_num);
                if risk <= MIN_MAINTAINED_RISK {
                    break;
                }
                risks.push_front(risk);
                epoch_num -= 1;
                count += 1;
            }

            if risks.is_empty() {
                epoch_num = 0;
            } else {
                epoch_num += 1;
            }

            let mut finality = self.finality_manager.write();
            finality.lowest_epoch_num = epoch_num;
            finality.risks_less_than = risks;
        }
    }

    /// Wait for the generation and the execution completion of a block in the
    /// consensus graph. This API is used mainly for testing purpose
    pub fn wait_for_generation(&self, hash: &H256) {
        while !self.inner.read().indices.contains_key(hash) {
            sleep(Duration::from_millis(1));
        }
        let best_state_block = self.inner.read().best_state_block_hash();
        self.executor.wait_for_result(best_state_block);
    }

    /// Determine whether the next mined block should have adaptive weight or
    /// not
    pub fn check_mining_adaptive_block(
        &self, inner: &mut ConsensusGraphInner, parent_hash: &H256,
        difficulty: &U256,
    ) -> bool
    {
        let parent_index = *inner.indices.get(parent_hash).unwrap();
        inner.check_mining_adaptive_block(parent_index, *difficulty)
    }

    pub fn get_height_from_epoch_number(
        &self, epoch_number: EpochNumber,
    ) -> Result<usize, String> {
        self.inner.read().get_height_from_epoch_number(epoch_number)
    }

    pub fn best_epoch_number(&self) -> usize {
        self.inner.read().best_epoch_number()
    }

    pub fn verified_invalid(&self, hash: &H256) -> bool {
        self.invalid_blocks.read().contains(hash)
    }

    pub fn invalidate_block(&self, hash: &H256) {
        self.invalid_blocks.write().insert(hash.clone());
    }

    pub fn get_block_total_weight(&self, hash: &H256) -> Option<i128> {
        let w = self.inner.write();
        if let Some(idx) = w.indices.get(hash).cloned() {
            Some(w.weight_tree.get(idx))
        } else {
            None
        }
    }

    pub fn get_block_epoch_number(&self, hash: &H256) -> Option<usize> {
        self.inner.read().get_block_epoch_number(hash)
    }

    pub fn get_block_hashes_by_epoch(
        &self, epoch_number: EpochNumber,
    ) -> Result<Vec<H256>, String> {
        self.inner.read().block_hashes_by_epoch(epoch_number)
    }

    pub fn gas_price(&self) -> Option<U256> {
        let inner = self.inner.read();
        let mut last_epoch_number = inner.best_epoch_number();
        let mut number_of_blocks_to_sample = GAS_PRICE_BLOCK_SAMPLE_SIZE;
        let mut tx_hashes = HashSet::new();
        let mut prices = Vec::new();

        loop {
            if number_of_blocks_to_sample == 0 || last_epoch_number == 0 {
                break;
            }
            if prices.len() == GAS_PRICE_TRANSACTION_SAMPLE_SIZE {
                break;
            }
            let mut hashes = inner
                .block_hashes_by_epoch(EpochNumber::Number(
                    last_epoch_number.into(),
                ))
                .unwrap();
            hashes.reverse();
            last_epoch_number -= 1;

            for hash in hashes {
                let block = self.data_man.block_by_hash(&hash, false).unwrap();
                for tx in block.transactions.iter() {
                    if tx_hashes.insert(tx.hash()) {
                        prices.push(tx.gas_price().clone());
                        if prices.len() == GAS_PRICE_TRANSACTION_SAMPLE_SIZE {
                            break;
                        }
                    }
                }
                number_of_blocks_to_sample -= 1;
                if number_of_blocks_to_sample == 0 {
                    break;
                }
            }
        }

        prices.sort();
        if prices.is_empty() {
            None
        } else {
            Some(prices[prices.len() / 2])
        }
    }

    pub fn get_balance(
        &self, address: H160, epoch_number: EpochNumber,
    ) -> Result<U256, String> {
        self.inner
            .read()
            .get_balance_validated(address, epoch_number)
    }

    pub fn get_epoch_blocks(
        &self, inner: &ConsensusGraphInner, epoch_index: usize,
    ) -> Vec<Arc<Block>> {
        inner.get_epoch_blocks(&self.data_man, epoch_index)
    }

    // TODO Merge logic.
    /// This is a very expensive call to force the engine to recompute the state
    /// root of a given block
    pub fn compute_state_for_block(
        &self, block_hash: &H256, inner: &mut ConsensusGraphInner,
    ) -> (StateRootWithAuxInfo, H256) {
        // If we already computed the state of the block before, we should not
        // do it again
        // FIXME: propagate the error up
        debug!("compute_state_for_block {:?}", block_hash);
        {
            let maybe_cached_state = self
                .data_man
                .storage_manager
                .get_state_no_commit(block_hash.clone())
                .unwrap();
            match maybe_cached_state {
                Some(cached_state) => {
                    if let Some(receipts_root) =
                        self.data_man.get_receipts_root(&block_hash)
                    {
                        return (
                            cached_state.get_state_root().unwrap().unwrap(),
                            receipts_root,
                        );
                    }
                }
                None => {}
            }
        }
        // FIXME: propagate the error up
        let me: usize = inner.indices.get(block_hash).unwrap().clone();
        let block_height = inner.arena[me].height as usize;
        let mut fork_height = block_height;
        let mut chain: Vec<usize> = Vec::new();
        let mut idx = me;
        while fork_height > 0
            && (fork_height >= inner.pivot_chain.len()
                || inner.pivot_chain[fork_height] != idx)
        {
            chain.push(idx);
            fork_height -= 1;
            idx = inner.arena[idx].parent;
        }
        // Because we have genesis at height 0, this should always be true
        debug_assert!(inner.pivot_chain[fork_height] == idx);
        debug!("Forked at index {} height {}", idx, fork_height);
        chain.push(idx);
        chain.reverse();
        let start_index = inner.find_start_index(&chain);
        debug!("Start execution from index {}", start_index);

        // We need the state of the fork point to start executing the fork
        if start_index != 0 {
            let mut last_state_height = if inner.pivot_chain.len()
                > DEFERRED_STATE_EPOCH_COUNT as usize
            {
                inner.pivot_chain.len() - DEFERRED_STATE_EPOCH_COUNT as usize
            } else {
                0
            };

            last_state_height += 1;
            while last_state_height <= fork_height {
                let epoch_index = inner.pivot_chain[last_state_height];
                let reward_execution_info = inner.get_reward_execution_info(
                    &self.data_man,
                    last_state_height,
                    &inner.pivot_chain,
                );
                self.executor.enqueue_epoch(EpochExecutionTask::new(
                    inner.arena[epoch_index].hash,
                    inner.get_epoch_block_hashes(epoch_index),
                    reward_execution_info,
                    false,
                    false,
                ));
                last_state_height += 1;
            }
        }

        for fork_at in start_index..chain.len() {
            let epoch_index = chain[fork_at];
            let reward_index = if fork_height + fork_at
                > REWARD_EPOCH_COUNT as usize
            {
                let epoch_num =
                    fork_height + fork_at - REWARD_EPOCH_COUNT as usize;
                let anticone_penalty_cutoff_epoch_num =
                    epoch_num + ANTICONE_PENALTY_UPPER_EPOCH_COUNT as usize;
                let pivot_block_upper =
                    if anticone_penalty_cutoff_epoch_num > fork_height {
                        chain[anticone_penalty_cutoff_epoch_num - fork_height]
                    } else {
                        inner.pivot_chain[anticone_penalty_cutoff_epoch_num]
                    };
                let pivot_index = if epoch_num > fork_height {
                    chain[epoch_num - fork_height]
                } else {
                    inner.pivot_chain[epoch_num]
                };
                Some((pivot_index, pivot_block_upper))
            } else {
                None
            };
            let reward_execution_info = inner
                .get_reward_execution_info_from_index(
                    &self.data_man,
                    reward_index,
                );
            self.executor.enqueue_epoch(EpochExecutionTask::new(
                inner.arena[epoch_index].hash,
                inner.get_epoch_block_hashes(epoch_index),
                reward_execution_info,
                false,
                false,
            ));
        }

        // FIXME: Propagate errors upward
        let (state_root, receipts_root) =
            self.executor.wait_for_result(*block_hash);
        debug!(
            "Epoch {:?} has state_root={:?} receipts_root={:?}",
            inner.arena[me].hash, state_root, receipts_root
        );

        (state_root, receipts_root)
    }

    /// Force the engine to recompute the deferred state root for a particular
    /// block given a delay.
    pub fn compute_deferred_state_for_block(
        &self, block_hash: &H256, delay: usize,
    ) -> (StateRootWithAuxInfo, H256) {
        let inner = &mut *self.inner.write();

        // FIXME: Propagate errors upward
        let mut idx = inner.indices.get(block_hash).unwrap().clone();
        for _i in 0..delay {
            if idx == inner.genesis_block_index {
                break;
            }
            idx = inner.arena[idx].parent;
        }
        let hash = inner.arena[idx].hash;
        self.compute_state_for_block(&hash, inner)
    }

    fn log_debug_epoch_computation(
        &self, epoch_index: usize, inner: &ConsensusGraphInner,
    ) -> ComputeEpochDebugRecord {
        let epoch_block_hash = inner.arena[epoch_index].hash;

        let epoch_block_hashes = inner.get_epoch_block_hashes(epoch_index);

        // Parent state root.
        let parent_index = inner.arena[epoch_index].parent;
        let parent_block_hash = inner.arena[parent_index].hash;
        let parent_state_root = inner
            .data_man
            .storage_manager
            .get_state_no_commit(parent_block_hash)
            .unwrap()
            // Unwrapping is safe because the state exists.
            .unwrap()
            .get_state_root()
            .unwrap()
            .unwrap();

        // Recompute epoch.
        let anticone_cut_height =
            REWARD_EPOCH_COUNT - ANTICONE_PENALTY_UPPER_EPOCH_COUNT;
        let mut anticone_penalty_cutoff_epoch_block = parent_index;
        for _i in 1..anticone_cut_height {
            if anticone_penalty_cutoff_epoch_block == NULL {
                break;
            }
            anticone_penalty_cutoff_epoch_block =
                inner.arena[anticone_penalty_cutoff_epoch_block].parent;
        }
        let mut reward_epoch_block = anticone_penalty_cutoff_epoch_block;
        for _i in 0..ANTICONE_PENALTY_UPPER_EPOCH_COUNT {
            if reward_epoch_block == NULL {
                break;
            }
            reward_epoch_block = inner.arena[reward_epoch_block].parent;
        }
        let reward_index = if reward_epoch_block == NULL {
            None
        } else {
            Some((reward_epoch_block, anticone_penalty_cutoff_epoch_block))
        };

        let reward_execution_info = inner
            .get_reward_execution_info_from_index(&self.data_man, reward_index);
        let task = EpochExecutionTask::new(
            epoch_block_hash,
            epoch_block_hashes.clone(),
            reward_execution_info,
            false,
            true,
        );
        let debug_record_data = task.debug_record.clone();
        {
            let mut debug_record_data_locked = debug_record_data.lock();
            let debug_record = debug_record_data_locked.as_mut().unwrap();

            debug_record.parent_block_hash = parent_block_hash;
            debug_record.parent_state_root = parent_state_root;
            debug_record.reward_epoch_hash = if reward_epoch_block != NULL {
                Some(inner.arena[reward_epoch_block].hash)
            } else {
                None
            };
            debug_record.anticone_penalty_cutoff_epoch_hash =
                if anticone_penalty_cutoff_epoch_block != NULL {
                    Some(inner.arena[anticone_penalty_cutoff_epoch_block].hash)
                } else {
                    None
                };

            let blocks = epoch_block_hashes
                .iter()
                .map(|hash| self.data_man.block_by_hash(hash, false).unwrap())
                .collect::<Vec<_>>();

            debug_record.block_hashes = epoch_block_hashes;
            debug_record.block_txs = blocks
                .iter()
                .map(|block| block.transactions.len())
                .collect::<Vec<_>>();;
            debug_record.transactions = blocks
                .iter()
                .flat_map(|block| block.transactions.clone())
                .collect::<Vec<_>>();

            debug_record.block_authors = blocks
                .iter()
                .map(|block| *block.block_header.author())
                .collect::<Vec<_>>();
        }
        self.executor.enqueue_epoch(task);
        self.executor.wait_for_result(epoch_block_hash);

        Arc::try_unwrap(debug_record_data)
            .unwrap()
            .into_inner()
            .unwrap()
    }

    fn log_invalid_state_root(
        &self, expected_state_root: &StateRootWithAuxInfo,
        got_state_root: (&StateRoot, &StateRootAuxInfo), deferred: usize,
        inner: &ConsensusGraphInner,
    ) -> std::io::Result<()>
    {
        let debug_record = self.log_debug_epoch_computation(deferred, inner);
        let debug_record_rlp = debug_record.rlp_bytes();

        let deferred_block_hash = inner.arena[deferred].hash;

        warn!(
            "Invalid state root: should be {:?}, got {:?}, deferred block: {:?}, \
            reward epoch bock: {:?}, anticone cutoff block: {:?}, \
            number of blocks in epoch: {:?}, number of transactions in epoch: {:?}, rewards: {:?}",
            expected_state_root,
            got_state_root,
            deferred_block_hash,
            debug_record.reward_epoch_hash,
            debug_record.anticone_penalty_cutoff_epoch_hash,
            debug_record.block_hashes.len(),
            debug_record.transactions.len(),
            debug_record.merged_rewards_by_author,
        );

        let dump_dir = &self.conf.debug_dump_dir_invalid_state_root;
        let invalid_state_root_path =
            dump_dir.clone() + &deferred_block_hash.hex();
        std::fs::create_dir_all(dump_dir)?;

        if std::path::Path::new(&invalid_state_root_path).exists() {
            return Ok(());
        }
        let mut file = std::fs::File::create(&invalid_state_root_path)?;
        file.write_all(&debug_record_rlp)?;

        Ok(())
    }

    fn check_block_full_validity(
        &self, new: usize, block: &Block, inner: &mut ConsensusGraphInner,
        adaptive: bool, anticone_barrier: &BitSet,
        weight_tuple: Option<&(Vec<i128>, Vec<i128>, Vec<i128>)>,
    ) -> bool
    {
        let parent = inner.arena[new].parent;
        if inner.arena[parent].data.partial_invalid {
            warn!(
                "Partially invalid due to partially invalid parent. {:?}",
                block.block_header.clone()
            );
            return false;
        }

        // Check whether the new block select the correct parent block
        if !inner.check_correct_parent(new, anticone_barrier, weight_tuple) {
            warn!(
                "Partially invalid due to picking incorrect parent. {:?}",
                block.block_header.clone()
            );
            return false;
        }

        // Check whether difficulty is set correctly
        if inner.arena[new].difficulty
            != inner.expected_difficulty(&inner.arena[parent].hash)
        {
            warn!(
                "Partially invalid due to wrong difficulty. {:?}",
                block.block_header.clone()
            );
            return false;
        }

        // Check adaptivity match. Note that in bench mode we do not check
        // the adaptive field correctness. We simply override its value
        // with the right one.
        if !self.conf.bench_mode {
            if inner.arena[new].adaptive != adaptive {
                warn!(
                    "Partially invalid due to invalid adaptive field. {:?}",
                    block.block_header.clone()
                );
                return false;
            }
        }

        // Check if the state root is correct or not
        // TODO: We may want to optimize this because now on the chain switch we
        // are going to compute state twice
        let state_root_valid = if block.block_header.height()
            < DEFERRED_STATE_EPOCH_COUNT
        {
            *block.block_header.deferred_state_root()
                == inner.genesis_block_state_root
                && *block.block_header.deferred_receipts_root()
                    == inner.genesis_block_receipts_root
        } else {
            let mut deferred = new;
            for _ in 0..DEFERRED_STATE_EPOCH_COUNT {
                deferred = inner.arena[deferred].parent;
            }
            debug_assert!(
                block.block_header.height() - DEFERRED_STATE_EPOCH_COUNT
                    == inner.arena[deferred].height
            );
            debug!("Deferred block is {:?}", inner.arena[deferred].hash);

            let correct_receipts_root =
                self.data_man.get_receipts_root(&inner.arena[deferred].hash);
            if self
                .data_man
                .storage_manager
                .contains_state(inner.arena[deferred].hash)
                && correct_receipts_root.is_some()
            {
                let mut valid = true;
                let correct_state_root = self
                    .data_man
                    .storage_manager
                    .get_state_no_commit(inner.arena[deferred].hash)
                    .unwrap()
                    // Unwrapping is safe because the state exists.
                    .unwrap()
                    .get_state_root()
                    .unwrap()
                    .unwrap();
                if *block.block_header.deferred_state_root()
                    != correct_state_root.state_root
                {
                    self.log_invalid_state_root(
                        &correct_state_root,
                        block.block_header.deferred_state_root_with_aux_info(),
                        deferred,
                        inner,
                    )
                    .ok();
                    valid = false;
                }
                if *block.block_header.deferred_receipts_root()
                    != correct_receipts_root.unwrap()
                {
                    warn!(
                        "Invalid receipt root: should be {:?}",
                        correct_receipts_root
                    );
                    valid = false;
                }
                valid
            } else {
                // Call the expensive function to check this state root
                let deferred_hash = inner.arena[deferred].hash;
                let (state_root, receipts_root) =
                    self.compute_state_for_block(&deferred_hash, inner);

                if state_root.state_root
                    != *block.block_header.deferred_state_root()
                {
                    self.log_invalid_state_root(
                        &state_root,
                        block.block_header.deferred_state_root_with_aux_info(),
                        deferred,
                        inner,
                    )
                    .ok();
                }

                *block.block_header.deferred_state_root()
                    == state_root.state_root
                    && *block.block_header.deferred_receipts_root()
                        == receipts_root
            }
        };

        if !state_root_valid {
            warn!(
                "Partially invalid in fork due to deferred block. me={:?}",
                block.block_header.clone()
            );
            return false;
        }
        return true;
    }

    /// Recompute metadata associated information on pivot chain changes
    fn recompute_metadata(
        &self, inner: &mut ConsensusGraphInner, start_at: usize,
        mut to_update: HashSet<usize>,
    )
    {
        inner
            .pivot_chain_metadata
            .resize_with(inner.pivot_chain.len(), Default::default);
        for i in start_at..inner.pivot_chain.len() {
            let me = inner.pivot_chain[i];
            inner.arena[me].last_pivot_in_past = i;
            inner.pivot_chain_metadata[i]
                .last_pivot_in_past_blocks
                .clear();
            inner.pivot_chain_metadata[i]
                .last_pivot_in_past_blocks
                .insert(me);
            to_update.remove(&me);
        }
        let mut stack = Vec::new();
        let to_visit = to_update.clone();
        for i in &to_update {
            stack.push((0, *i));
        }
        while !stack.is_empty() {
            let (stage, me) = stack.pop().unwrap();
            if !to_visit.contains(&me) {
                continue;
            }
            let parent = inner.arena[me].parent;
            if stage == 0 {
                if to_update.contains(&me) {
                    to_update.remove(&me);
                    stack.push((1, me));
                    stack.push((0, parent));
                    for referee in &inner.arena[me].referees {
                        stack.push((0, *referee));
                    }
                }
            } else if stage == 1 && me != 0 {
                let mut last_pivot = inner.arena[parent].last_pivot_in_past;
                for referee in &inner.arena[me].referees {
                    let x = inner.arena[*referee].last_pivot_in_past;
                    last_pivot = max(last_pivot, x);
                }
                inner.arena[me].last_pivot_in_past = last_pivot;
                inner.pivot_chain_metadata[last_pivot]
                    .last_pivot_in_past_blocks
                    .insert(me);
            }
        }
    }

    /// construct_pivot() should be used after on_new_block_construction_only()
    /// calls. It builds the pivot chain and ists state at once, avoiding
    /// intermediate redundant computation triggered by on_new_block().
    pub fn construct_pivot(&self) {
        {
            let mut inner = &mut *self.inner.write();

            assert_eq!(inner.pivot_chain.len(), 1);
            assert_eq!(inner.pivot_chain[0], inner.genesis_block_index);

            let mut new_pivot_chain = Vec::new();
            let mut u = inner.genesis_block_index;
            loop {
                new_pivot_chain.push(u);
                let mut heaviest = NULL;
                let mut heaviest_weight = 0;
                for index in &inner.arena[u].children {
                    let weight = inner.weight_tree.get(*index);
                    if heaviest == NULL
                        || ConsensusGraphInner::is_heavier(
                            (weight, &inner.arena[*index].hash),
                            (heaviest_weight, &inner.arena[heaviest].hash),
                        )
                    {
                        heaviest = *index;
                        heaviest_weight = weight;
                    }
                }
                if heaviest == NULL {
                    break;
                }
                u = heaviest;
            }

            // Construct epochs
            let mut height = 1;
            while height < new_pivot_chain.len() {
                // First, identify all the blocks in the current epoch
                let mut queue = Vec::new();
                {
                    let copy_of_fork_at = height;
                    let enqueue_if_new =
                        |inner: &mut ConsensusGraphInner,
                         queue: &mut Vec<usize>,
                         index| {
                            if inner.arena[index].data.epoch_number == NULL {
                                inner.arena[index].data.epoch_number =
                                    copy_of_fork_at;
                                queue.push(index);
                            }
                        };

                    let mut at = 0;
                    enqueue_if_new(inner, &mut queue, new_pivot_chain[height]);
                    while at < queue.len() {
                        let me = queue[at];
                        let tmp = inner.arena[me].referees.clone();
                        for referee in tmp {
                            enqueue_if_new(inner, &mut queue, referee);
                        }
                        enqueue_if_new(
                            inner,
                            &mut queue,
                            inner.arena[me].parent,
                        );
                        at += 1;
                    }
                }

                // Construct in-memory receipts root
                if new_pivot_chain.len() >= DEFERRED_STATE_EPOCH_COUNT as usize
                    && height
                        < new_pivot_chain.len()
                            - DEFERRED_STATE_EPOCH_COUNT as usize
                {
                    // This block's deferred block is pivot_index, so the
                    // deferred_receipts_root in its header is the
                    // receipts_root of pivot_index
                    let future_block_hash = inner.arena[new_pivot_chain
                        [height + DEFERRED_STATE_EPOCH_COUNT as usize]]
                        .hash
                        .clone();
                    self.data_man.insert_receipts_root(
                        inner.arena[new_pivot_chain[height]].hash,
                        self.data_man
                            .block_header_by_hash(&future_block_hash)
                            .unwrap()
                            .deferred_receipts_root()
                            .clone(),
                    );
                }

                height += 1;
            }

            // If the db is not corrupted, all unwrap in the following should
            // pass.
            // TODO Verify db state in case of data missing
            // TODO Recompute missing data if needed
            inner
                .adjust_difficulty(*new_pivot_chain.last().expect("not empty"));
            inner.pivot_chain = new_pivot_chain;

            // Now we construct pivot_chain_metadata and compute
            // last_pivot_in_past
            let mut metadata_to_update = HashSet::new();
            for i in 1..inner.arena.len() {
                metadata_to_update.insert(i);
            }
            self.recompute_metadata(inner, 0, metadata_to_update);
        }
        {
            let inner = &*self.inner.read();
            // Compute receipts root for the deferred block of the mining block,
            // which is not in the db
            if inner.pivot_chain.len() > DEFERRED_STATE_EPOCH_COUNT as usize {
                let state_height = inner.pivot_chain.len()
                    - DEFERRED_STATE_EPOCH_COUNT as usize;
                let pivot_index = inner.pivot_chain[state_height];
                let pivot_hash = inner.arena[pivot_index].hash.clone();
                let epoch_indexes =
                    &inner.arena[pivot_index].data.ordered_epoch_blocks;
                let mut epoch_receipts =
                    Vec::with_capacity(epoch_indexes.len());

                let mut receipts_correct = true;
                for i in epoch_indexes {
                    if let Some(r) =
                        self.data_man.block_results_by_hash_with_epoch(
                            &inner.arena[*i].hash,
                            &pivot_hash,
                            true,
                        )
                    {
                        epoch_receipts.push(r.receipts);
                    } else {
                        // Constructed pivot chain does not match receipts in
                        // db, so we have to recompute
                        // the receipts of this epoch
                        receipts_correct = false;
                        break;
                    }
                }
                if receipts_correct {
                    self.data_man.insert_receipts_root(
                        pivot_hash,
                        BlockHeaderBuilder::compute_block_receipts_root(
                            &epoch_receipts,
                        ),
                    );
                } else {
                    let reward_execution_info = inner
                        .get_reward_execution_info(
                            &self.data_man,
                            state_height,
                            &inner.pivot_chain,
                        );
                    let epoch_block_hashes = inner.get_epoch_block_hashes(
                        inner.pivot_chain[state_height],
                    );
                    self.executor.compute_epoch(EpochExecutionTask::new(
                        pivot_hash,
                        epoch_block_hashes,
                        reward_execution_info,
                        true,
                        false,
                    ));
                }
            }
        }
    }

    /// Subroutine called by on_new_block() and on_new_block_construction_only()
    fn insert_block_initial(
        &self, inner: &mut ConsensusGraphInner, block: Arc<Block>,
    ) -> usize {
        let (me, indices_len) = inner.insert(block.as_ref());
        self.statistics
            .set_consensus_graph_inserted_block_count(indices_len);
        me
    }

    /// Subroutine called by on_new_block() and on_new_block_construction_only()
    fn update_lcts_initial(&self, inner: &mut ConsensusGraphInner, me: usize) {
        let parent = inner.arena[me].parent;

        inner.weight_tree.make_tree(me);
        inner.weight_tree.link(parent, me);
        inner.inclusive_weight_tree.make_tree(me);
        inner.inclusive_weight_tree.link(parent, me);
        inner.stable_weight_tree.make_tree(me);
        inner.stable_weight_tree.link(parent, me);

        inner.stable_tree.make_tree(me);
        inner.stable_tree.link(parent, me);
        inner.stable_tree.set(
            me,
            (inner.inner_conf.adaptive_weight_alpha_num as i128)
                * (inner.block_weight(parent, false)
                    + inner.arena[parent].past_era_weight),
        );

        inner.adaptive_tree.make_tree(me);
        inner.adaptive_tree.link(parent, me);
        let parent_w = inner.weight_tree.get(parent);
        inner.adaptive_tree.set(
            me,
            -parent_w * (inner.inner_conf.adaptive_weight_alpha_num as i128),
        );

        inner.inclusive_adaptive_tree.make_tree(me);
        inner.inclusive_adaptive_tree.link(parent, me);
        let parent_iw = inner.inclusive_weight_tree.get(parent);
        inner.inclusive_adaptive_tree.set(
            me,
            -parent_iw * (inner.inner_conf.adaptive_weight_alpha_num as i128),
        );
    }

    /// Subroutine called by on_new_block() and on_new_block_construction_only()
    fn update_lcts_finalize(
        &self, inner: &mut ConsensusGraphInner, me: usize, stable: bool,
    ) -> i128 {
        let parent = inner.arena[me].parent;
        let weight = inner.block_weight(me, false);
        let inclusive_weight = inner.block_weight(me, true);

        inner.weight_tree.path_apply(me, weight);
        inner.inclusive_weight_tree.path_apply(me, inclusive_weight);
        if stable {
            inner.stable_weight_tree.path_apply(me, weight);
        }

        inner.stable_tree.path_apply(
            me,
            (inner.inner_conf.adaptive_weight_alpha_den as i128) * weight,
        );
        if stable {
            inner.adaptive_tree.path_apply(
                me,
                (inner.inner_conf.adaptive_weight_alpha_den as i128) * weight,
            );
        }
        inner.adaptive_tree.catepillar_apply(
            parent,
            -weight * (inner.inner_conf.adaptive_weight_alpha_num as i128),
        );

        inner.inclusive_adaptive_tree.path_apply(
            me,
            (inner.inner_conf.adaptive_weight_alpha_den as i128)
                * inclusive_weight,
        );
        inner.inclusive_adaptive_tree.catepillar_apply(
            parent,
            -inclusive_weight
                * (inner.inner_conf.adaptive_weight_alpha_num as i128),
        );

        weight
    }

    /// Preliminarily check whether a block is partially invalid or not due to
    /// incorrect parent selection!
    fn preliminary_check_validity(
        &self, inner: &mut ConsensusGraphInner, me: usize,
    ) -> bool {
        let last = inner.pivot_chain.last().cloned().unwrap();
        let parent = inner.arena[me].parent;
        if last == parent {
            return true;
        }
        let lca = inner.weight_tree.lca(last, me);
        let fork_at = inner.arena[lca].height as usize + 1;
        if fork_at >= inner.pivot_chain.len() {
            return true;
        }
        let a = inner.weight_tree.ancestor_at(me, fork_at as usize);
        let s = inner.pivot_chain[fork_at];

        let mut last_pivot = inner.arena[parent].last_pivot_in_past;
        for referee in &inner.arena[me].referees {
            last_pivot =
                max(last_pivot, inner.arena[*referee].last_pivot_in_past);
        }
        let total_weight = inner.weight_tree.get(inner.genesis_block_index);
        let before_last_epoch_weight =
            inner.arena[inner.pivot_chain[last_pivot]].past_weight;
        let subtree_s_weight = inner.weight_tree.get(s);

        let lower_bound_s_weight =
            before_last_epoch_weight + subtree_s_weight - total_weight;
        if lower_bound_s_weight < 0 {
            return true;
        }
        let estimate_weight = inner.block_weight(me, false);
        let upper_bound_a_weight = inner.weight_tree.get(a) + estimate_weight;
        return upper_bound_a_weight >= lower_bound_s_weight;
    }

    /// This is the function to insert a new block into the consensus graph
    /// during construction. We by pass many verifications because those
    /// blocks are from our own database so we trust them. After inserting
    /// all blocks with this function, we need to call construct_pivot() to
    /// finish the building from db!ss
    pub fn on_new_block_construction_only(&self, hash: &H256) {
        let block = self.data_man.block_by_hash(hash, false).unwrap();

        let inner = &mut *self.inner.write();

        let me = self.insert_block_initial(inner, block.clone());

        let anticone_barrier = inner.compute_anticone(me);
        let weight_tuple = if anticone_barrier.len() >= ANTICONE_BARRIER_CAP {
            Some(inner.compute_subtree_weights(me, &anticone_barrier))
        } else {
            None
        };
        let fully_valid = if let Some(partial_invalid) =
            self.data_man.block_status_from_db(hash)
        {
            !partial_invalid
        } else {
            // FIXME If the status of a block close to terminals is missing
            // (likely to happen) and we try to check its validity with the
            // commented code, we will recompute the whole DAG from genesis
            // because the pivot chain is empty now, which is not what we
            // want for fast recovery. A better solution is
            // to assume it's partial invalid, construct the pivot chain and
            // other data like block_receipts_root first, and then check its
            // full validity. The pivot chain might need to be updated
            // depending on the validity result.

            // The correct logic here should be as follows, but this
            // solution is very costly
            // ```
            // let valid = self.check_block_full_validity(me, &block, inner, sync_inner);
            // self.insert_block_status_to_db(hash, !valid);
            // valid
            // ```

            // The assumed value should be false after we fix this issue.
            // Now we optimistically hope that they are valid.
            debug!("Assume block {} is valid", hash);
            true
        };
        if !fully_valid {
            inner.arena[me].data.partial_invalid = true;
            return;
        }

        self.update_lcts_initial(inner, me);

        let (stable, adaptive) =
            inner.adaptive_weight(me, &anticone_barrier, weight_tuple.as_ref());
        inner.arena[me].stable = stable;
        inner.arena[me].adaptive = adaptive;

        self.update_lcts_finalize(inner, me, stable);
    }

    /// This is the main function that SynchronizationGraph calls to deliver a
    /// new block to the consensus graph.
    pub fn on_new_block(&self, hash: &H256) {
        let block = self.data_man.block_by_hash(hash, true).unwrap();

        debug!(
            "insert new block into consensus: block_header={:?} tx_count={}, block_size={}",
            block.block_header,
            block.transactions.len(),
            block.size(),
        );

        let mut inner = &mut *self.inner.write();

        let me = self.insert_block_initial(inner, block.clone());

        // It's only correct to set tx stale after the block is considered
        // terminal for mining.
        self.txpool.set_tx_packed(block.transactions.clone());

        let anticone_barrier = inner.compute_anticone(me);

        let weight_tuple = if anticone_barrier.len() >= ANTICONE_BARRIER_CAP {
            Some(inner.compute_subtree_weights(me, &anticone_barrier))
        } else {
            None
        };

        self.update_lcts_initial(inner, me);

        let (stable, adaptive) =
            inner.adaptive_weight(me, &anticone_barrier, weight_tuple.as_ref());

        let fully_valid = if self.preliminary_check_validity(inner, me) {
            self.check_block_full_validity(
                me,
                block.as_ref(),
                inner,
                adaptive,
                &anticone_barrier,
                weight_tuple.as_ref(),
            )
        } else {
            false
        };
        self.data_man.insert_block_status_to_db(hash, !fully_valid);

        if !fully_valid {
            inner.arena[me].data.partial_invalid = true;
            debug!(
                "Block {} (hash = {}) is partially invalid",
                me, inner.arena[me].hash
            );
        } else {
            debug!(
                "Block {} (hash = {}) is fully valid",
                me, inner.arena[me].hash
            );
        }

        inner.arena[me].stable = stable;
        // FIXME: Is this necessary?
        inner.arena[me].adaptive = adaptive;

        let mut extend_pivot = false;
        let mut fork_at = inner.pivot_chain.len() + 1;
        let old_pivot_chain_len = inner.pivot_chain.len();
        if fully_valid {
            let my_weight = self.update_lcts_finalize(inner, me, stable);

            self.aggregate_total_weight_in_past(my_weight);

            let last = inner.pivot_chain.last().cloned().unwrap();
            fork_at = if inner.arena[me].parent == last {
                inner.pivot_chain.push(me);
                inner.pivot_chain_metadata.push(Default::default());
                extend_pivot = true;
                old_pivot_chain_len
            } else {
                let lca = inner.weight_tree.lca(last, me);

                let fork_at = inner.arena[lca].height as usize + 1;
                let prev = inner.pivot_chain[fork_at];
                let prev_weight = inner.weight_tree.get(prev);
                let new = inner.weight_tree.ancestor_at(me, fork_at as usize);
                let new_weight = inner.weight_tree.get(new);

                if ConsensusGraphInner::is_heavier(
                    (new_weight, &inner.arena[new].hash),
                    (prev_weight, &inner.arena[prev].hash),
                ) {
                    // The new subtree is heavier, update pivot chain
                    inner.pivot_chain.truncate(fork_at);
                    let mut u = new;
                    loop {
                        inner.pivot_chain.push(u);
                        let mut heaviest = NULL;
                        let mut heaviest_weight = 0;
                        for index in &inner.arena[u].children {
                            let weight = inner.weight_tree.get(*index);
                            if heaviest == NULL
                                || ConsensusGraphInner::is_heavier(
                                    (weight, &inner.arena[*index].hash),
                                    (
                                        heaviest_weight,
                                        &inner.arena[heaviest].hash,
                                    ),
                                )
                            {
                                heaviest = *index;
                                heaviest_weight = weight;
                            }
                        }
                        if heaviest == NULL {
                            break;
                        }
                        u = heaviest;
                    }
                    fork_at
                } else {
                    // The previous subtree is still heavier, nothing is updated
                    debug!("Finish Consensus.on_new_block() with pivot chain unchanged");
                    old_pivot_chain_len
                }
            };
            debug!("Forked at index {}", inner.pivot_chain[fork_at - 1]);

            if fork_at < old_pivot_chain_len {
                let enqueue_if_obsolete =
                    |inner: &mut ConsensusGraphInner,
                     queue: &mut VecDeque<usize>,
                     index| {
                        let epoch_number = inner.arena[index].data.epoch_number;
                        if epoch_number != NULL && epoch_number >= fork_at {
                            inner.arena[index].data.epoch_number = NULL;
                            queue.push_back(index);
                        }
                    };

                let mut queue = VecDeque::new();
                enqueue_if_obsolete(inner, &mut queue, last);
                while let Some(me) = queue.pop_front() {
                    let tmp = inner.arena[me].referees.clone();
                    for referee in tmp {
                        enqueue_if_obsolete(inner, &mut queue, referee);
                    }
                    enqueue_if_obsolete(
                        inner,
                        &mut queue,
                        inner.arena[me].parent,
                    );
                }
            }

            assert_ne!(fork_at, 0);

            // Construct epochs
            let mut pivot_index = fork_at;
            while pivot_index < inner.pivot_chain.len() {
                // First, identify all the blocks in the current epoch
                let mut queue = Vec::new();
                {
                    let copy_of_fork_at = pivot_index;
                    let enqueue_if_new =
                        |inner: &mut ConsensusGraphInner,
                         queue: &mut Vec<usize>,
                         index| {
                            if inner.arena[index].data.epoch_number == NULL {
                                inner.arena[index].data.epoch_number =
                                    copy_of_fork_at;
                                queue.push(index);
                            }
                        };

                    let mut at = 0;
                    enqueue_if_new(
                        inner,
                        &mut queue,
                        inner.pivot_chain[pivot_index],
                    );
                    while at < queue.len() {
                        let me = queue[at];
                        let tmp = inner.arena[me].referees.clone();
                        for referee in tmp {
                            enqueue_if_new(inner, &mut queue, referee);
                        }
                        enqueue_if_new(
                            inner,
                            &mut queue,
                            inner.arena[me].parent,
                        );
                        at += 1;
                    }
                }
                pivot_index += 1;
            }
        }

        // Now compute last_pivot_in_block and update pivot_metadata.
        // Note that we need to do this for partially invalid blocks to
        // propagate information!
        if !extend_pivot {
            let update_at = fork_at - 1;
            let mut last_pivot_to_update = HashSet::new();
            last_pivot_to_update.insert(me);
            for pivot_index in update_at..old_pivot_chain_len {
                for x in &inner.pivot_chain_metadata[pivot_index]
                    .last_pivot_in_past_blocks
                {
                    last_pivot_to_update.insert(*x);
                }
            }
            self.recompute_metadata(inner, fork_at, last_pivot_to_update);
        } else {
            let height = inner.arena[me].height as usize;
            inner.arena[me].last_pivot_in_past = height;
            inner.pivot_chain_metadata[height]
                .last_pivot_in_past_blocks
                .insert(me);
            //            inner
            //                .pivot_future_weights
            //                .add(height,
            // &SignedBigNum::pos(inner.block_weight(me)));
        }

        // Now we can safely return
        if !fully_valid {
            return;
        }

        let to_state_pos = if inner.pivot_chain.len()
            < DEFERRED_STATE_EPOCH_COUNT as usize
        {
            0 as usize
        } else {
            inner.pivot_chain.len() - DEFERRED_STATE_EPOCH_COUNT as usize + 1
        };

        let mut state_at = fork_at;
        if fork_at + DEFERRED_STATE_EPOCH_COUNT as usize > old_pivot_chain_len {
            if old_pivot_chain_len > DEFERRED_STATE_EPOCH_COUNT as usize {
                state_at = old_pivot_chain_len
                    - DEFERRED_STATE_EPOCH_COUNT as usize
                    + 1;
            } else {
                state_at = 1;
            }
        }

        // Apply transactions in the determined total order
        while state_at < to_state_pos {
            let epoch_index = inner.pivot_chain[state_at];
            let reward_execution_info = inner.get_reward_execution_info(
                &self.data_man,
                state_at,
                &inner.pivot_chain,
            );
            self.executor.enqueue_epoch(EpochExecutionTask::new(
                inner.arena[epoch_index].hash,
                inner.get_epoch_block_hashes(epoch_index),
                reward_execution_info,
                true,
                false,
            ));
            state_at += 1;
        }

        inner.adjust_difficulty(*inner.pivot_chain.last().expect("not empty"));

        self.update_confirmation_risks(inner, self.get_total_weight_in_past());
        inner.optimistic_executed_height = if to_state_pos > 0 {
            Some(to_state_pos)
        } else {
            None
        };
        inner.persist_terminals();
        debug!("Finish processing block in ConsensusGraph: hash={:?}", hash);
    }

    pub fn best_block_hash(&self) -> H256 {
        self.inner.read().best_block_hash()
    }

    pub fn best_state_epoch_number(&self) -> usize {
        self.inner.read().best_state_epoch_number()
    }

    pub fn get_hash_from_epoch_number(
        &self, epoch_number: EpochNumber,
    ) -> Result<H256, String> {
        self.inner.read().get_hash_from_epoch_number(epoch_number)
    }

    pub fn get_transaction_info_by_hash(
        &self, hash: &H256,
    ) -> Option<(SignedTransaction, Receipt, TransactionAddress)> {
        // We need to hold the inner lock to ensure that tx_address and receipts
        // are consistent
        let inner = self.inner.read();
        if let Some((receipt, address)) =
            inner.get_transaction_receipt_with_address(hash)
        {
            let block =
                self.data_man.block_by_hash(&address.block_hash, false)?;
            let transaction = (*block.transactions[address.index]).clone();
            Some((transaction, receipt, address))
        } else {
            None
        }
    }

    pub fn transaction_count(
        &self, address: H160, epoch_number: EpochNumber,
    ) -> Result<U256, String> {
        self.inner.read().transaction_count(address, epoch_number)
    }

    pub fn get_ancestor(&self, hash: &H256, n: usize) -> H256 {
        let inner = self.inner.write();
        let me = *inner.indices.get(hash).unwrap();
        let idx = inner.weight_tree.ancestor_at(me, n);
        inner.arena[idx].hash.clone()
    }

    pub fn try_get_best_state(&self) -> Option<State> {
        self.inner.read().try_get_best_state(&self.data_man)
    }

    /// Wait until the best state has been executed, and return the state
    pub fn get_best_state(&self) -> State {
        let inner = self.inner.read();
        self.wait_for_block_state(&inner.best_state_block_hash());
        inner
            .try_get_best_state(&self.data_man)
            .expect("Best state has been executed")
    }

    /// Returns the total number of blocks in consensus graph
    pub fn block_count(&self) -> usize { self.inner.read().indices.len() }

    pub fn estimate_gas(&self, tx: &SignedTransaction) -> Result<U256, String> {
        self.call_virtual(tx, EpochNumber::LatestState)
            .map(|(_, gas_used)| gas_used)
    }

    pub fn logs(
        &self, filter: Filter,
    ) -> Result<Vec<LocalizedLogEntry>, FilterError> {
        let block_hashes = if filter.block_hashes.is_none() {
            if filter.from_epoch >= filter.to_epoch {
                return Err(FilterError::InvalidEpochNumber {
                    from_epoch: filter.from_epoch,
                    to_epoch: filter.to_epoch,
                });
            }

            let inner = self.inner.read();

            if filter.from_epoch >= inner.pivot_chain.len() {
                return Ok(Vec::new());
            }

            let from_epoch = filter.from_epoch;
            let to_epoch = min(filter.to_epoch, inner.pivot_chain.len());

            let blooms = filter.bloom_possibilities();
            let bloom_match = |block_log_bloom: &Bloom| {
                blooms
                    .iter()
                    .any(|bloom| block_log_bloom.contains_bloom(bloom))
            };

            let mut blocks = Vec::new();
            for epoch_idx in from_epoch..to_epoch {
                let epoch_hash = inner.arena[epoch_idx].hash;
                for index in &inner.arena[inner.pivot_chain[epoch_idx]]
                    .data
                    .ordered_epoch_blocks
                {
                    let hash = inner.arena[*index].hash;
                    if let Some(block_log_bloom) = self
                        .data_man
                        .block_results_by_hash_with_epoch(
                            &hash,
                            &epoch_hash,
                            false,
                        )
                        .map(|r| r.bloom)
                    {
                        if !bloom_match(&block_log_bloom) {
                            continue;
                        }
                    }
                    blocks.push(hash);
                }
            }

            blocks
        } else {
            filter.block_hashes.as_ref().unwrap().clone()
        };

        Ok(self.logs_from_blocks(
            block_hashes,
            |entry| filter.matches(entry),
            filter.limit,
        ))
    }

    /// Returns logs matching given filter. The order of logs returned will be
    /// the same as the order of the blocks provided. And it's the callers
    /// responsibility to sort blocks provided in advance.
    pub fn logs_from_blocks<F>(
        &self, mut blocks: Vec<H256>, matches: F, limit: Option<usize>,
    ) -> Vec<LocalizedLogEntry>
    where
        F: Fn(&LogEntry) -> bool + Send + Sync,
        Self: Sized,
    {
        // sort in reverse order
        blocks.reverse();

        let mut logs = blocks
            .chunks(128)
            .flat_map(move |blocks_chunk| {
                blocks_chunk.into_par_iter()
                    .filter_map(|hash|
                        self.inner.read().block_receipts_by_hash(&hash, false).map(|r| (hash, (*r).clone()))
                    )
                    .filter_map(|(hash, receipts)| self.data_man.block_by_hash(&hash, false).map(|b| (hash, receipts, b.transaction_hashes())))
                    .flat_map(|(hash, mut receipts, mut hashes)| {
                        if receipts.len() != hashes.len() {
                            warn!("Block ({}) has different number of receipts ({}) to transactions ({}). Database corrupt?", hash, receipts.len(), hashes.len());
                            assert!(false);
                        }
                        let mut log_index = receipts.iter().fold(0, |sum, receipt| sum + receipt.logs.len());

                        let receipts_len = receipts.len();
                        hashes.reverse();
                        receipts.reverse();
                        receipts.into_iter()
                            .map(|receipt| receipt.logs)
                            .zip(hashes)
                            .enumerate()
                            .flat_map(move |(index, (mut logs, tx_hash))| {
                                let current_log_index = log_index;
                                let no_of_logs = logs.len();
                                log_index -= no_of_logs;

                                logs.reverse();
                                logs.into_iter()
                                    .enumerate()
                                    .map(move |(i, log)| LocalizedLogEntry {
                                        entry: log,
                                        block_hash: *hash,
                                        transaction_hash: tx_hash,
                                        // iterating in reverse order
                                        transaction_index: receipts_len - index - 1,
                                        transaction_log_index: no_of_logs - i - 1,
                                        log_index: current_log_index - i - 1,
                                    })
                            })
                            .filter(|log_entry| matches(&log_entry.entry))
                            .take(limit.unwrap_or(::std::usize::MAX))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
            .take(limit.unwrap_or(::std::usize::MAX))
            .collect::<Vec<LocalizedLogEntry>>();
        logs.reverse();
        logs
    }

    pub fn call_virtual(
        &self, tx: &SignedTransaction, epoch: EpochNumber,
    ) -> Result<(Vec<u8>, U256), String> {
        // only allow to call against stated epoch
        self.inner.read().validate_stated_epoch(&epoch)?;
        let epoch_id = self.get_hash_from_epoch_number(epoch)?;
        self.executor.call_virtual(tx, &epoch_id)
    }

    /// Wait for a block's epoch is computed.
    /// Return the state_root and receipts_root
    pub fn wait_for_block_state(
        &self, block_hash: &H256,
    ) -> (StateRootWithAuxInfo, H256) {
        self.executor.wait_for_result(*block_hash)
    }
}

impl Drop for ConsensusGraph {
    fn drop(&mut self) { self.executor.stop(); }
}
