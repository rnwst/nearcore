use crate::network_protocol::{PartialEdgeInfo, Edge, EdgeState, RoutingTableUpdate};
use crate::stats::metrics;
use crate::time;
use crate::types::{ReasonForBan};
use near_primitives::network::{AnnounceAccount,PeerId};
use std::sync::Arc;
use super::NetworkState;
use crate::peer_manager::peer_manager_actor::Event;

impl NetworkState {
    pub async fn add_accounts(self: &Arc<NetworkState>, accounts: Vec<AnnounceAccount>) {
        let this = self.clone();
        self.spawn(async move {
            let new_accounts = this.routing_table_view.add_accounts(accounts);
            tracing::debug!(target: "network", account_id = ?this.config.validator.as_ref().map(|v|v.account_id()), ?new_accounts, "Received new accounts");
            this.broadcast_routing_table_update(Arc::new(RoutingTableUpdate::from_accounts(
                new_accounts.clone(),
            )));
            this.config.event_sink.push(Event::AccountsAdded(new_accounts));
        }).await.unwrap()
    }

    pub fn propose_edge(&self, peer1: &PeerId, with_nonce: Option<u64>) -> PartialEdgeInfo {
        // When we create a new edge we increase the latest nonce by 2 in case we miss a removal
        // proposal from our partner.
        let nonce = with_nonce.unwrap_or_else(|| {
            self.routing_table_view.get_local_edge(peer1).map_or(1, |edge| edge.next())
        });
        PartialEdgeInfo::new(&self.config.node_id(), peer1, nonce, &self.config.node_key)
    }

    pub async fn finalize_edge(
        self: &Arc<Self>,
        clock: &time::Clock,
        peer_id: PeerId,
        edge_info: PartialEdgeInfo,
    ) -> Result<Edge, ReasonForBan> {
        let edge = Edge::build_with_secret_key(
            self.config.node_id(),
            peer_id.clone(),
            edge_info.nonce,
            &self.config.node_key,
            edge_info.signature,
        );
        self.add_edges(&clock, vec![edge.clone()]).await?;
        Ok(edge)
    }

    pub async fn add_edges(
        self: &Arc<Self>,
        clock: &time::Clock,
        edges: Vec<Edge>,
    ) -> Result<(), ReasonForBan> {
        // TODO(gprusak): sending duplicate edges should be considered a malicious behavior
        // instead, however that would be backward incompatible, so it can be introduced in
        // PROTOCOL_VERSION 60 earliest.
        let (edges,ok) = self.graph.verify(edges).await; 
        let this = self.clone();
        let clock = clock.clone();
        let _ = self.add_edges_demux.call(edges, |edges:Vec<Vec<Edge>>| async move {
            let results : Vec<_> = edges.iter().map(|_|()).collect();
            let edges : Vec<_> = edges.into_iter().flatten().collect();
            let (mut edges, next_hops) = this.graph.update_routing_table(&clock,edges).await;
            this.routing_table_view.add_local_edges(&edges);
            // TODO: pruned_edges are not passed any more
            this.routing_table_view.update(&[],next_hops);
            // Don't send tombstones during the initial time.
            // Most of the network is created during this time, which results
            // in us sending a lot of tombstones to peers.
            // Later, the amount of new edges is a lot smaller.
            if let Some(skip_tombstones_duration) = this.config.skip_tombstones {
                if clock.now() < this.created_at + skip_tombstones_duration {
                    edges.retain(|edge| edge.edge_type() == EdgeState::Active);
                    metrics::EDGE_TOMBSTONE_SENDING_SKIPPED.inc();
                }
            }
            // Broadcast new edges to all other peers.
            this.broadcast_routing_table_update(Arc::new(RoutingTableUpdate::from_edges(
                edges,
            )));
            results
        }).await;
        match ok {
            true => Ok(()),
            false => Err(ReasonForBan::InvalidEdge),
        }
    }
}
