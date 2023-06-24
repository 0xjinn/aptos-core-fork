// Copyright © Aptos Foundation

use crate::{
    sharded_block_executor::sharded_executor_client::ShardedExecutorClient, AptosVM,
    ShardedBlockExecutor, VMExecutor,
};
use aptos_block_partitioner::sharded_block_partitioner::ShardedBlockPartitioner;
use aptos_infallible::Mutex;
use aptos_language_e2e_tests::{
    account::AccountData, common_transactions::peer_to_peer_txn, executor::FakeExecutor,
};
use aptos_types::{
    state_store::state_key::StateKeyInner,
    transaction::{analyzed_transaction::AnalyzedTransaction, Transaction, TransactionOutput},
};
use move_core_types::account_address::AccountAddress;
use std::sync::Arc;

fn generate_account_at(executor: &mut FakeExecutor, address: AccountAddress) -> AccountData {
    executor.new_account_data_at(address)
}

fn generate_non_conflicting_sender_receiver(
    executor: &mut FakeExecutor,
) -> (AccountData, AccountData) {
    let sender = executor.create_raw_account_data(3_000_000_000, 0);
    let receiver = executor.create_raw_account_data(3_000_000_000, 0);
    executor.add_account_data(&sender);
    executor.add_account_data(&receiver);
    (sender, receiver)
}

fn generate_non_conflicting_p2p(
    executor: &mut FakeExecutor,
) -> (AnalyzedTransaction, AccountData, AccountData) {
    let (mut sender, receiver) = generate_non_conflicting_sender_receiver(executor);
    let transfer_amount = 1_000;
    let txn = generate_p2p_txn(&mut sender, &receiver, transfer_amount);
    // execute transaction
    (txn, sender, receiver)
}

fn generate_p2p_txn(
    sender: &mut AccountData,
    receiver: &AccountData,
    transfer_amount: u64,
) -> AnalyzedTransaction {
    let txn = Transaction::UserTransaction(peer_to_peer_txn(
        sender.account(),
        receiver.account(),
        sender.sequence_number(),
        transfer_amount,
        100,
    ))
    .into();
    sender.increment_sequence_number();
    txn
}

fn compare_txn_outputs(
    unsharded_txn_output: Vec<TransactionOutput>,
    sharded_txn_output: Vec<TransactionOutput>,
) {
    assert_eq!(unsharded_txn_output.len(), sharded_txn_output.len());
    for i in 0..unsharded_txn_output.len() {
        assert_eq!(
            unsharded_txn_output[i].status(),
            sharded_txn_output[i].status()
        );
        assert_eq!(
            unsharded_txn_output[i].gas_used(),
            sharded_txn_output[i].gas_used()
        );
        //assert_eq!(unsharded_txn_output[i].write_set(), sharded_txn_output[i].write_set());
        assert_eq!(
            unsharded_txn_output[i].events(),
            sharded_txn_output[i].events()
        );
        // Global supply tracking for coin is not supported in sharded execution yet, so we filter
        // out the table item from the write set, which has the global supply. This is a hack until
        // we support global supply tracking in sharded execution.
        let unsharded_write_set_without_table_item = unsharded_txn_output[i]
            .write_set()
            .into_iter()
            .filter(|(k, _)| matches!(k.inner(), &StateKeyInner::AccessPath(_)))
            .collect::<Vec<_>>();
        let sharded_write_set_without_table_item = sharded_txn_output[i]
            .write_set()
            .into_iter()
            .filter(|(k, _)| matches!(k.inner(), &StateKeyInner::AccessPath(_)))
            .collect::<Vec<_>>();
        assert_eq!(
            unsharded_write_set_without_table_item,
            sharded_write_set_without_table_item
        );
    }
}

#[test]
fn test_sharded_block_executor_no_conflict() {
    let num_txns = 400;
    let num_shards = 8;
    let mut executor = FakeExecutor::from_head_genesis();
    let mut transactions = Vec::new();
    for _ in 0..num_txns {
        transactions.push(generate_non_conflicting_p2p(&mut executor).0)
    }
    let partitioner = ShardedBlockPartitioner::new(num_shards);
    let partitioned_txns = partitioner.partition(transactions.clone(), 1);
    let executor_clients =
        ShardedExecutorClient::create_sharded_executor_clients(num_shards, Some(2));
    let sharded_block_executor = ShardedBlockExecutor::new(executor_clients);
    let sharded_txn_output = sharded_block_executor
        .execute_block(
            Arc::new(executor.data_store().clone()),
            partitioned_txns,
            2,
            None,
        )
        .unwrap();
    let unsharded_txn_output = AptosVM::execute_block(
        transactions.into_iter().map(|t| t.into_txn()).collect(),
        &executor.data_store(),
        None,
    )
    .unwrap();
    compare_txn_outputs(unsharded_txn_output, sharded_txn_output);
}

#[test]
#[ignore]
// Sharded execution with cross shard conflict doesn't work for now because we don't have
// cross round dependency tracking yet.
fn test_sharded_block_executor_with_conflict() {
    let num_txns = 8;
    let num_shards = 2;
    let num_accounts = 4;
    let mut executor = FakeExecutor::from_head_genesis();
    let mut transactions = Vec::new();
    let mut accounts = Vec::new();
    for _ in 0..num_accounts {
        let account = generate_account_at(&mut executor, AccountAddress::random());
        accounts.push(Mutex::new(account));
    }
    for i in 1..num_txns / num_accounts {
        for j in 0..num_accounts {
            let sender = &mut accounts[j].lock();
            let receiver = &accounts[(j + i) % num_accounts].lock();
            let transfer_amount = 1_000;
            let txn = generate_p2p_txn(sender, receiver, transfer_amount);
            transactions.push(txn)
        }
    }

    let partitioner = ShardedBlockPartitioner::new(num_shards);
    let partitioned_txns = partitioner.partition(transactions.clone(), 1);
    let executor_clients =
        ShardedExecutorClient::create_sharded_executor_clients(num_shards, Some(2));
    let sharded_block_executor = ShardedBlockExecutor::new(executor_clients);
    let _sharded_txn_output = sharded_block_executor
        .execute_block(
            Arc::new(executor.data_store().clone()),
            partitioned_txns,
            2,
            None,
        )
        .unwrap();
}