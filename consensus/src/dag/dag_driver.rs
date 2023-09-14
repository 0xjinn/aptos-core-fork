// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use super::{
    adapter::TLedgerInfoProvider,
    dag_fetcher::FetchRequester,
    order_rule::OrderRule,
    storage::DAGStorage,
    types::{CertifiedAck, CertifiedNodeMessage, DAGMessage, Extensions},
    RpcHandler,
};
use crate::{
    dag::{
        dag_fetcher::TFetchRequester,
        dag_state_sync::DAG_WINDOW,
        dag_store::Dag,
        types::{CertificateAckState, CertifiedNode, Node, SignatureBuilder},
    },
    payload_manager::PayloadManager,
    state_replication::PayloadClient,
};
use anyhow::bail;
use aptos_consensus_types::common::{Author, PayloadFilter};
use aptos_infallible::RwLock;
use aptos_logger::{debug, error};
use aptos_reliable_broadcast::ReliableBroadcast;
use aptos_time_service::{TimeService, TimeServiceTrait};
use aptos_types::{block_info::Round, epoch_state::EpochState};
use async_trait::async_trait;
use futures::{
    executor::block_on,
    future::{AbortHandle, Abortable},
    FutureExt,
};
use std::{sync::Arc, time::Duration};
use thiserror::Error as ThisError;
use tokio_retry::strategy::ExponentialBackoff;

#[derive(Debug, ThisError)]
pub enum DagDriverError {
    #[error("missing parents")]
    MissingParents,
}

pub(crate) struct DagDriver {
    author: Author,
    epoch_state: Arc<EpochState>,
    dag: Arc<RwLock<Dag>>,
    payload_manager: Arc<PayloadManager>,
    payload_client: Arc<dyn PayloadClient>,
    reliable_broadcast: Arc<ReliableBroadcast<DAGMessage, ExponentialBackoff>>,
    current_round: Round,
    time_service: TimeService,
    rb_abort_handle: Option<AbortHandle>,
    storage: Arc<dyn DAGStorage>,
    order_rule: OrderRule,
    fetch_requester: Arc<FetchRequester>,
    ledger_info_provider: Arc<dyn TLedgerInfoProvider>,
}

impl DagDriver {
    pub fn new(
        author: Author,
        epoch_state: Arc<EpochState>,
        dag: Arc<RwLock<Dag>>,
        payload_manager: Arc<PayloadManager>,
        payload_client: Arc<dyn PayloadClient>,
        reliable_broadcast: Arc<ReliableBroadcast<DAGMessage, ExponentialBackoff>>,
        time_service: TimeService,
        storage: Arc<dyn DAGStorage>,
        order_rule: OrderRule,
        fetch_requester: Arc<FetchRequester>,
        ledger_info_provider: Arc<dyn TLedgerInfoProvider>,
    ) -> Self {
        let pending_node = storage
            .get_pending_node()
            .expect("should be able to read dag storage");
        let highest_round = dag.read().highest_round();
        let highest_strong_links_round = dag
            .read()
            .get_strong_links_for_round(highest_round, &epoch_state.verifier)
            .map_or_else(|| highest_round.saturating_sub(1), |_| highest_round);

        debug!(
            "highest_round: {}, current_round: {}",
            highest_round, highest_strong_links_round
        );

        let mut driver = Self {
            author,
            epoch_state,
            dag,
            payload_manager,
            payload_client,
            reliable_broadcast,
            current_round: highest_strong_links_round,
            time_service,
            rb_abort_handle: None,
            storage,
            order_rule,
            fetch_requester,
            ledger_info_provider,
        };

        // If we were broadcasting the node for the round already, resume it
        if let Some(node) =
            pending_node.filter(|node| node.round() == highest_strong_links_round + 1)
        {
            driver.current_round = node.round();
            driver.broadcast_node(node);
        } else {
            // kick start a new round
            block_on(driver.enter_new_round(highest_strong_links_round + 1));
        }
        driver
    }

    pub async fn add_node(&mut self, node: CertifiedNode) -> anyhow::Result<()> {
        let highest_strong_links_round = {
            let mut dag_writer = self.dag.write();

            if !dag_writer.all_exists(node.parents_metadata()) {
                if let Err(err) = self.fetch_requester.request_for_certified_node(node) {
                    error!("request to fetch failed: {}", err);
                }
                bail!(DagDriverError::MissingParents);
            }

            self.payload_manager
                .prefetch_payload_data(node.payload(), node.metadata().timestamp());
            dag_writer.add_node(node)?;

            let highest_round = dag_writer.highest_round();
            dag_writer
                .get_strong_links_for_round(highest_round, &self.epoch_state.verifier)
                .map_or_else(|| highest_round.saturating_sub(1), |_| highest_round)
        };

        if self.current_round <= highest_strong_links_round {
            self.enter_new_round(highest_strong_links_round + 1).await;
        }
        Ok(())
    }

    pub async fn enter_new_round(&mut self, new_round: Round) {
        debug!("entering new round {}", new_round);
        let strong_links = self
            .dag
            .read()
            .get_strong_links_for_round(new_round - 1, &self.epoch_state.verifier)
            .expect("Strong links should exist");
        let payload_filter = {
            let dag_reader = self.dag.read();
            let highest_commit_round = self
                .ledger_info_provider
                .get_highest_committed_anchor_round();
            if strong_links.is_empty() {
                PayloadFilter::Empty
            } else {
                PayloadFilter::from(
                    &dag_reader
                        .reachable(
                            strong_links.iter().map(|node| node.metadata()),
                            Some(highest_commit_round.saturating_sub(DAG_WINDOW as u64)),
                            |_| true,
                        )
                        .map(|node_status| node_status.as_node().payload())
                        .collect(),
                )
            }
        };
        let payload = match self
            .payload_client
            .pull_payload(
                Duration::from_secs(1),
                1000,
                10 * 1024 * 1024,
                payload_filter,
                Box::pin(async {}),
                false,
                0,
                0.0,
            )
            .await
        {
            Ok(payload) => payload,
            Err(e) => {
                // TODO: return empty payload instead
                panic!("error pulling payload: {}", e);
            },
        };
        // TODO: need to wait to pass median of parents timestamp
        let timestamp = self.time_service.now_unix_time();
        self.current_round = new_round;
        let new_node = Node::new(
            self.epoch_state.epoch,
            self.current_round,
            self.author,
            timestamp.as_micros() as u64,
            payload,
            strong_links,
            Extensions::empty(),
        );
        self.storage
            .save_pending_node(&new_node)
            .expect("node must be saved");
        self.broadcast_node(new_node);
    }

    pub fn broadcast_node(&mut self, node: Node) {
        let rb = self.reliable_broadcast.clone();
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        let signature_builder =
            SignatureBuilder::new(node.metadata().clone(), self.epoch_state.clone());
        let cert_ack_set = CertificateAckState::new(self.epoch_state.verifier.len());
        let latest_ledger_info = self.ledger_info_provider.get_latest_ledger_info();
        let round = node.round();
        let core_task = self
            .reliable_broadcast
            .broadcast(node.clone(), signature_builder)
            .then(move |certificate| {
                let certified_node = CertifiedNode::new(node, certificate.signatures().to_owned());
                let certified_node_msg =
                    CertifiedNodeMessage::new(certified_node, latest_ledger_info);
                rb.broadcast(certified_node_msg, cert_ack_set)
            });
        let task = async move {
            debug!("Start reliable broadcast for round {}", round);
            core_task.await;
            debug!("Finish reliable broadcast for round {}", round);
        };
        tokio::spawn(Abortable::new(task, abort_registration));
        if let Some(prev_handle) = self.rb_abort_handle.replace(abort_handle) {
            prev_handle.abort();
        }
    }
}

#[async_trait]
impl RpcHandler for DagDriver {
    type Request = CertifiedNode;
    type Response = CertifiedAck;

    async fn process(&mut self, node: Self::Request) -> anyhow::Result<Self::Response> {
        let epoch = node.metadata().epoch();
        {
            let dag_reader = self.dag.read();
            if dag_reader.exists(node.metadata()) {
                return Ok(CertifiedAck::new(epoch));
            }
        }

        let node_metadata = node.metadata().clone();
        self.add_node(node)
            .await
            .map(|_| self.order_rule.process_new_node(&node_metadata))?;

        Ok(CertifiedAck::new(epoch))
    }
}
