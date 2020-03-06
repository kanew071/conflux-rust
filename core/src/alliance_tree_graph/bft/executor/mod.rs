// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, ensure, format_err, Result};
use libra_config::config::NodeConfig;
use libra_crypto::{
    hash::{GENESIS_BLOCK_ID, PRE_GENESIS_BLOCK_ID},
    HashValue,
};
use libra_types::{
    block_info::{BlockInfo, PivotBlockDecision, Round},
    contract_event::ContractEvent,
    crypto_proxies::{
        LedgerInfoWithSignatures, NextValidatorSetProposal, ValidatorSet,
        ValidatorVerifier,
    },
    ledger_info::LedgerInfo,
    transaction::{
        Transaction, TransactionOutput, TransactionPayload, TransactionStatus,
    },
    validator_verifier::VerifyError,
    vm_error::{StatusCode, VMStatus},
    write_set::WriteSet,
};
use libradb::LibraDB;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

const GENESIS_EPOCH: u64 = 0;
const GENESIS_ROUND: Round = 0;

/// A structure that summarizes the result of the execution needed for consensus
/// to agree on. The execution is responsible for generating the ID of the new
/// state, which is returned in the result.
///
/// Not every transaction in the payload succeeds: the returned vector keeps the
/// boolean status of success / failure of the transactions.
/// Note that the specific details of compute_status are opaque to
/// StateMachineReplication, which is going to simply pass the results between
/// StateComputer and TxnManager.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct StateComputeResult {
    pub executed_state: ExecutedState,
    /* The compute status (success/failure) of the given payload. The
     * specific details are opaque for StateMachineReplication, which is
     * merely passing it between StateComputer and TxnManager.
     *pub compute_status: Vec<TransactionStatus>, */
}

impl StateComputeResult {
    /*
    pub fn version(&self) -> Version {
        self.executed_state.version
    }

    pub fn root_hash(&self) -> HashValue {
        self.executed_state.state_id
    }

    pub fn status(&self) -> &Vec<TransactionStatus> {
        &self.compute_status
    }
    */

    pub fn has_reconfiguration(&self) -> bool {
        self.executed_state.validators.is_some()
    }
}

/// Executed state derived from StateComputeResult that is maintained with every
/// proposed block. `state_id`(transaction accumulator root hash) summarized
/// both the information of the version and the validators.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutedState {
    /// Tracks the last pivot selection of a proposed block
    pub pivot: Option<PivotBlockDecision>,
    /// Tracks the execution state of a proposed block
    //pub state_id: HashValue,
    /// Version of after executing a proposed block.  This state must be
    /// persisted to ensure that on restart that the version is calculated
    /// correctly
    //pub version: Version,
    /// If set, this is the validator set that should be changed to if this
    /// block is committed. TODO [Reconfiguration] the validators are
    /// currently ignored, no reconfiguration yet.
    pub validators: Option<ValidatorSet>,
}

/*
impl ExecutedState {
    pub fn state_for_genesis() -> Self {
        ExecutedState {
            pivot: None,
            //state_id: *ACCUMULATOR_PLACEHOLDER_HASH,
            //version: 0,
            validators: None,
        }
    }
}
*/

/// Generated by processing VM's output.
#[derive(Debug, Clone)]
pub struct ProcessedVMOutput {
    /// The entire set of data associated with each transaction.
    //transaction_data: Vec<TransactionData>,

    /// The in-memory Merkle Accumulator and state Sparse Merkle Tree after
    /// appending all the transactions in this set.
    //executed_trees: ExecutedTrees,

    /// If set, this is the validator set that should be changed to if this
    /// block is committed. TODO [Reconfiguration] the validators are
    /// currently ignored, no reconfiguration yet.
    pub validators: Option<ValidatorSet>,
    /// If set, this is the selected pivot block in current transaction.
    pub pivot_block: Option<PivotBlockDecision>,
    /// Whether the pivot_block is the updated value by executing this block.
    pub pivot_updated: bool,
}

impl ProcessedVMOutput {
    pub fn new(
        //transaction_data: Vec<TransactionData>,
        //executed_trees: ExecutedTrees,
        validators: Option<ValidatorSet>,
        pivot_block: Option<PivotBlockDecision>,
        pivot_updated: bool,
    ) -> Self
    {
        ProcessedVMOutput {
            //transaction_data,
            //executed_trees,
            validators,
            pivot_block,
            pivot_updated,
        }
    }

    /*
    pub fn transaction_data(&self) -> &[TransactionData] {
        &self.transaction_data
    }

    pub fn executed_trees(&self) -> &ExecutedTrees {
        &self.executed_trees
    }

    pub fn accu_root(&self) -> HashValue {
        self.executed_trees().txn_accumulator().root_hash()
    }

    pub fn version(&self) -> Option<Version> {
        self.executed_trees().version()
    }
    */

    pub fn validators(&self) -> &Option<ValidatorSet> { &self.validators }

    pub fn pivot_block(&self) -> &Option<PivotBlockDecision> {
        &self.pivot_block
    }

    pub fn pivot_updated(&self) -> bool { self.pivot_updated }

    // This method should only be called by tests.
    pub fn set_validators(&mut self, validator_set: ValidatorSet) {
        self.validators = Some(validator_set)
    }

    pub fn state_compute_result(&self) -> StateComputeResult {
        //let num_leaves =
        // self.executed_trees().txn_accumulator().num_leaves();
        // let version = if num_leaves == 0 { 0 } else { num_leaves - 1 };
        StateComputeResult {
            // Now that we have the root hash and execution status we can send
            // the response to consensus.
            // TODO: The VM will support a special transaction to set the
            // validators for the next epoch that is part of a block
            // execution.
            executed_state: ExecutedState {
                pivot: self.pivot_block.clone(),
                //state_id: self.accu_root(),
                //version,
                validators: self.validators.clone(),
            },
            /*
            compute_status: self
                .transaction_data()
                .iter()
                .map(|txn_data| txn_data.status())
                .cloned()
                .collect(),
                */
        }
    }
}

/// `Executor` implements all functionalities the execution module needs to
/// provide.
pub struct Executor {
    db: Arc<LibraDB>,
    administrators: RwLock<Option<ValidatorVerifier>>,
}

impl Executor {
    /// Constructs an `Executor`.
    pub fn new(config: &NodeConfig, db: Arc<LibraDB>) -> Self {
        let mut executor = Executor {
            db,
            administrators: RwLock::new(None),
        };

        if executor
            .db
            .get_startup_info()
            .expect("Shouldn't fail")
            .is_none()
        {
            let genesis_txn = config
                .execution
                .genesis
                .as_ref()
                .expect("failed to load genesis transaction!")
                .clone();
            executor.init_genesis(genesis_txn);
        }
        executor
    }

    /// This is used when we start for the first time and the DB is completely
    /// empty. It will write necessary information to DB by committing the
    /// genesis transaction.
    fn init_genesis(&mut self, genesis_txn: Transaction) {
        let genesis_txns = vec![genesis_txn];

        info!("PRE_GENESIS_BLOCK_ID: {}", *PRE_GENESIS_BLOCK_ID);

        // Create a block with genesis_txn being the only transaction. Execute
        // it then commit it immediately.
        // We create `PRE_GENESIS_BLOCK_ID` as the parent of the genesis block.
        let output = self
            .execute_block(
                genesis_txns.clone(),
                None, /* last_pivot */
                *PRE_GENESIS_BLOCK_ID,
                *GENESIS_BLOCK_ID,
                GENESIS_EPOCH,
            )
            .expect("Failed to execute genesis block.");

        let ledger_info = LedgerInfo::new(
            BlockInfo::new(
                GENESIS_EPOCH,
                GENESIS_ROUND,
                *PRE_GENESIS_BLOCK_ID,
                None,
                HashValue::zero(),
                0,
                0,
                output.validators().clone(),
            ),
            HashValue::zero(),
        );
        let ledger_info_with_sigs = LedgerInfoWithSignatures::new(
            ledger_info,
            /* signatures = */ BTreeMap::new(),
        );
        self.commit_blocks(
            vec![(genesis_txns, Arc::new(output))],
            ledger_info_with_sigs,
        )
        .expect("Failed to commit genesis block.");
        info!("GENESIS transaction is committed.")
    }

    pub fn get_libra_db(&self) -> Arc<LibraDB> { self.db.clone() }

    pub fn set_administrators(&self, admins: ValidatorVerifier) {
        let mut administrators = self.administrators.write();
        *administrators = Some(admins);
    }

    fn gen_output(events: Vec<ContractEvent>) -> TransactionOutput {
        let vm_status = VMStatus {
            major_status: StatusCode::EXECUTED,
            sub_status: None,
            message: None,
        };

        let status = TransactionStatus::Keep(vm_status);

        TransactionOutput::new(WriteSet::default(), events, 0, status)
    }

    /// Executes a block.
    pub fn execute_block(
        &self, transactions: Vec<Transaction>,
        last_pivot: Option<PivotBlockDecision>, parent_id: HashValue,
        id: HashValue, current_epoch: u64,
    ) -> Result<ProcessedVMOutput>
    {
        debug!(
            "Received request to execute block. Parent id: {:x}. Id: {:x}.",
            parent_id, id
        );

        ensure!(
            transactions.len() <= 2,
            "One block can at most contain 1 user transaction for proposal."
        );
        let mut vm_outputs = Vec::new();
        for transaction in transactions {
            // Execute the transaction
            match transaction {
                Transaction::BlockMetadata(_data) => {}
                Transaction::UserTransaction(trans) => {
                    let trans = trans.check_signature()?;
                    if trans.is_admin_type() {
                        // Check the voting power of signers in administrators.
                        let admins = self.administrators.read();
                        if admins.is_none() {
                            bail!("Administrators are not set.");
                        }
                        let admins = admins.as_ref().unwrap();
                        let signers = trans.pubkey_account_addresses();
                        match admins.check_voting_power(signers.iter()) {
                            Ok(_) => {}
                            Err(VerifyError::TooLittleVotingPower {
                                ..
                            }) => {
                                bail!("Not enough voting power in administrators.");
                            }
                            Err(_) => {
                                bail!(
                                    "There are signers not in administrators."
                                );
                            }
                        }
                    }
                    let payload = trans.payload();
                    let events = match payload {
                        TransactionPayload::WriteSet(change_set) => {
                            change_set.events().to_vec()
                        }
                        _ => bail!("Wrong transaction payload"),
                    };

                    ensure!(
                        events.len() == 1,
                        "One transaction can contain exactly 1 event."
                    );

                    let output = Self::gen_output(events);
                    vm_outputs.push(output);
                }
                Transaction::WriteSet(change_set) => {
                    let events = change_set.events().to_vec();
                    ensure!(
                        events.len() == 1,
                        "One transaction can contain exactly 1 event."
                    );

                    let output = Self::gen_output(events);
                    vm_outputs.push(output);
                }
            }
        }

        let status: Vec<_> = vm_outputs
            .iter()
            .map(TransactionOutput::status)
            .cloned()
            .collect();
        if !status.is_empty() {
            debug!("Execution status: {:?}", status);
        }

        let output =
            Self::process_vm_outputs(vm_outputs, last_pivot, current_epoch)
                .map_err(|err| {
                    format_err!("Failed to execute block: {}", err)
                })?;

        Ok(output)
    }

    /// Saves eligible blocks to persistent storage.
    /// If we have multiple blocks and not all of them have signatures, we may
    /// send them to storage in a few batches. For example, if we have
    /// ```text
    /// A <- B <- C <- D <- E
    /// ```
    /// and only `C` and `E` have signatures, we will send `A`, `B` and `C` in
    /// the first batch, then `D` and `E` later in the another batch.
    /// Commits a block and all its ancestors in a batch manner. Returns
    /// `Ok(())` if successful.
    pub fn commit_blocks(
        &self, _blocks: Vec<(Vec<Transaction>, Arc<ProcessedVMOutput>)>,
        ledger_info_with_sigs: LedgerInfoWithSignatures,
    ) -> Result<()>
    {
        info!(
            "Received request to commit block {:x}, round {}.",
            ledger_info_with_sigs.ledger_info().consensus_block_id(),
            ledger_info_with_sigs.ledger_info().round(),
        );

        self.db
            .save_ledger_info(&Some(ledger_info_with_sigs.clone()))?;
        Ok(())
    }

    pub fn get_epoch_change_ledger_infos(
        &self, start_epoch: u64, end_epoch: u64,
    ) -> Result<(Vec<LedgerInfoWithSignatures>, bool)> {
        self.db
            .get_epoch_change_ledger_infos(start_epoch, end_epoch)
    }

    /*
    /// Verifies the transactions based on the provided proofs and ledger info. If the transactions
    /// are valid, executes them and commits immediately if execution results match the proofs.
    pub fn execute_and_commit_chunk(
        &self,
        txn_list_with_proof: TransactionListWithProof,
        // Target LI that has been verified independently: the proofs are relative to this version.
        verified_target_li: LedgerInfoWithSignatures,
        // An optional end of epoch LedgerInfo. We do not allow chunks that end epoch without
        // carrying any epoch change LI.
        epoch_change_li: Option<LedgerInfoWithSignatures>,
        synced_trees: &mut ExecutedTrees,
    ) -> Result<()> {
        info!(
            "Local synced version: {}. First transaction version in request: {:?}. \
             Number of transactions in request: {}.",
            synced_trees.txn_accumulator().num_leaves() - 1,
            txn_list_with_proof.first_transaction_version,
            txn_list_with_proof.transactions.len(),
        );

        let (num_txns_to_skip, first_version) = Self::verify_chunk(
            &txn_list_with_proof,
            &verified_target_li,
            synced_trees.txn_accumulator().num_leaves(),
        )?;

        info!("Skipping the first {} transactions.", num_txns_to_skip);
        let transactions: Vec<_> = txn_list_with_proof
            .transactions
            .into_iter()
            .skip(num_txns_to_skip as usize)
            .collect();

        // Construct a StateView and pass the transactions to VM.
        let state_view = VerifiedStateView::new(
            Arc::clone(&self.storage_read_client),
            synced_trees.version(),
            synced_trees.state_root(),
            synced_trees.state_tree(),
        );
        let vm_outputs = {
            let _timer = OP_COUNTERS.timer("vm_execute_chunk_time_s");
            V::execute_block(transactions.to_vec(), &self.vm_config, &state_view)?
        };

        // Since other validators have committed these transactions, their status should all be
        // TransactionStatus::Keep.
        for output in &vm_outputs {
            if let TransactionStatus::Discard(_) = output.status() {
                bail!("Syncing transactions that should be discarded.");
            }
        }

        let (account_to_btree, account_to_proof) = state_view.into();

        let output = Self::process_vm_outputs(
            account_to_btree,
            account_to_proof,
            &transactions,
            vm_outputs,
            synced_trees,
        )?;

        // Since we have verified the proofs, we just need to verify that each TransactionInfo
        // object matches what we have computed locally.
        let mut txns_to_commit = vec![];
        for (txn, txn_data) in itertools::zip_eq(transactions, output.transaction_data()) {
            txns_to_commit.push(TransactionToCommit::new(
                txn,
                txn_data.account_blobs().clone(),
                txn_data.events().to_vec(),
                txn_data.gas_used(),
                txn_data.status().vm_status().major_status,
            ));
        }

        let ledger_info_to_commit =
            Self::find_chunk_li(verified_target_li, epoch_change_li, &output)?;
        if ledger_info_to_commit.is_none() && txns_to_commit.is_empty() {
            return Ok(());
        }
        self.storage_write_client.save_transactions(
            txns_to_commit,
            first_version,
            ledger_info_to_commit.clone(),
        )?;

        *synced_trees = output.executed_trees().clone();
        info!(
            "Synced to version {}, the corresponding LedgerInfo is {}.",
            synced_trees.version().expect("version must exist"),
            if ledger_info_to_commit.is_some() {
                "committed"
            } else {
                "not committed"
            },
        );
        Ok(())
    }

    /// In case there is a new LI to be added to a LedgerStore, verify and return it.
    fn find_chunk_li(
        verified_target_li: LedgerInfoWithSignatures,
        epoch_change_li: Option<LedgerInfoWithSignatures>,
        new_output: &ProcessedVMOutput,
    ) -> Result<Option<LedgerInfoWithSignatures>> {
        // If the chunk corresponds to the target LI, the target LI can be added to storage.
        if verified_target_li.ledger_info().version() == new_output.version().unwrap_or(0) {
            ensure!(
                verified_target_li
                    .ledger_info()
                    .transaction_accumulator_hash()
                    == new_output.accu_root(),
                "Root hash in target ledger info does not match local computation."
            );
            return Ok(Some(verified_target_li));
        }
        // If the epoch change LI is present, it must match the version of the chunk:
        // verify the version and the root hash.
        if let Some(epoch_change_li) = epoch_change_li {
            // Verify that the given ledger info corresponds to the new accumulator.
            ensure!(
                epoch_change_li.ledger_info().transaction_accumulator_hash()
                    == new_output.accu_root(),
                "Root hash of a given epoch LI does not match local computation."
            );
            ensure!(
                epoch_change_li.ledger_info().version() == new_output.version().unwrap_or(0),
                "Version of a given epoch LI does not match local computation."
            );
            ensure!(
                epoch_change_li.ledger_info().next_validator_set().is_some(),
                "Epoch change LI does not carry validator set"
            );
            ensure!(
                epoch_change_li.ledger_info().next_validator_set()
                    == new_output.validators().as_ref(),
                "New validator set of a given epoch LI does not match local computation"
            );
            return Ok(Some(epoch_change_li));
        }
        ensure!(
            new_output.validators.is_none(),
            "End of epoch chunk based on local computation but no EoE LedgerInfo provided."
        );
        Ok(None)
    }

    /// Verifies proofs using provided ledger info. Also verifies that the version of the first
    /// transaction matches the latest committed transaction. If the first few transaction happens
    /// to be older, returns how many need to be skipped and the first version to be committed.
    fn verify_chunk(
        txn_list_with_proof: &TransactionListWithProof,
        ledger_info_with_sigs: &LedgerInfoWithSignatures,
        num_committed_txns: u64,
    ) -> Result<(LeafCount, Version)> {
        txn_list_with_proof.verify(
            ledger_info_with_sigs.ledger_info(),
            txn_list_with_proof.first_transaction_version,
        )?;

        if txn_list_with_proof.transactions.is_empty() {
            return Ok((0, num_committed_txns as Version /* first_version */));
        }

        let first_txn_version = txn_list_with_proof
            .first_transaction_version
            .expect("first_transaction_version should exist.")
            as Version;

        ensure!(
            first_txn_version <= num_committed_txns,
            "Transaction list too new. Expected version: {}. First transaction version: {}.",
            num_committed_txns,
            first_txn_version
        );
        Ok((
            num_committed_txns - first_txn_version,
            num_committed_txns as Version,
        ))
    }
    */

    /// Post-processing of what the VM outputs. Returns the entire block's
    /// output.
    fn process_vm_outputs(
        vm_outputs: Vec<TransactionOutput>,
        last_pivot: Option<PivotBlockDecision>, current_epoch: u64,
    ) -> Result<ProcessedVMOutput>
    {
        ensure!(
            vm_outputs.len() <= 1,
            "One block can have at most one transaction output!"
        );

        let mut next_validator_set = None;
        let mut next_pivot_block = last_pivot;
        let mut pivot_updated = false;

        for vm_output in vm_outputs.into_iter() {
            let validator_set_change_event_key =
                ValidatorSet::change_event_key();
            let pivot_select_event_key =
                PivotBlockDecision::pivot_select_event_key();
            for event in vm_output.events() {
                // check for change in validator set
                if *event.key() == validator_set_change_event_key {
                    let next_validator_set_proposal =
                        NextValidatorSetProposal::from_bytes(
                            event.event_data(),
                        )?;
                    ensure!(
                        current_epoch == next_validator_set_proposal.this_epoch,
                        "Wrong epoch proposal."
                    );
                    next_validator_set =
                        Some(next_validator_set_proposal.next_validator_set);
                    break;
                }
                // check for pivot block selection.
                if *event.key() == pivot_select_event_key {
                    next_pivot_block = Some(PivotBlockDecision::from_bytes(
                        event.event_data(),
                    )?);
                    pivot_updated = true;
                    break;
                }
            }
        }

        Ok(ProcessedVMOutput::new(
            next_validator_set,
            next_pivot_block,
            pivot_updated,
        ))
    }
}
