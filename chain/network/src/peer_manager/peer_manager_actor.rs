use crate::client;
use crate::config;
use crate::network_protocol::{
    AccountOrPeerIdOrHash, Edge, EdgeState, PeerIdOrHash, PeerMessage, Ping, Pong,
    RawRoutedMessage, RoutedMessageBody, StateResponseInfo, SyncAccountsData,
};
use crate::peer::peer_actor::PeerActor;
use crate::peer_manager::connection;
use crate::peer_manager::network_state::NetworkState;
use crate::peer_manager::peer_store;
use crate::private_actix::{
    PeerRequestResult, PeerToManagerMsg, PeerToManagerMsgResp, PeersRequest, PeersResponse, StopMsg,
};
use crate::routing;
use crate::stats::metrics;
use crate::store;
use crate::tcp;
use crate::time;
use crate::types::{
    ConnectedPeerInfo, FullPeerInfo, GetNetworkInfo, KnownProducer, NetworkInfo, NetworkRequests,
    NetworkResponses, PeerManagerMessageRequest, PeerManagerMessageResponse, PeerType,
    ReasonForBan, SetChainInfo,
};
use actix::fut::future::wrap_future;
use actix::{Actor as _, ActorFutureExt as _, AsyncContext as _};
use anyhow::Context as _;
use near_performance_metrics_macros::perf;
use near_primitives::block::GenesisId;
use near_primitives::network::PeerId;
use rand::seq::IteratorRandom;
use rand::thread_rng;
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// How much time to wait (in milliseconds) after we send update nonce request before disconnecting.
/// This number should be large to handle pair of nodes with high latency.
const WAIT_ON_TRY_UPDATE_NONCE: time::Duration = time::Duration::milliseconds(6_000);
/// If we see an edge between us and other peer, but this peer is not a current connection, wait this
/// timeout and in case it didn't become a connected peer, broadcast edge removal update.
const WAIT_PEER_BEFORE_REMOVE: time::Duration = time::Duration::milliseconds(6_000);
/// Ratio between consecutive attempts to establish connection with another peer.
/// In the kth step node should wait `10 * EXPONENTIAL_BACKOFF_RATIO**k` milliseconds
const EXPONENTIAL_BACKOFF_RATIO: f64 = 1.1;
/// The initial waiting time between consecutive attempts to establish connection
const MONITOR_PEERS_INITIAL_DURATION: time::Duration = time::Duration::milliseconds(10);
/// How ofter should we broadcast edges.
const BROADCAST_VALIDATED_EDGES_INTERVAL: time::Duration = time::Duration::milliseconds(50);
/// Maximum amount of time spend processing edges.
const BROAD_CAST_EDGES_MAX_WORK_ALLOWED: time::Duration = time::Duration::milliseconds(50);
/// How often should we update the routing table
const UPDATE_ROUTING_TABLE_INTERVAL: time::Duration = time::Duration::milliseconds(1_000);
/// How often to report bandwidth stats.
const REPORT_BANDWIDTH_STATS_TRIGGER_INTERVAL: time::Duration =
    time::Duration::milliseconds(60_000);

/// If we received more than `REPORT_BANDWIDTH_THRESHOLD_BYTES` of data from given peer it's bandwidth stats will be reported.
const REPORT_BANDWIDTH_THRESHOLD_BYTES: usize = 10_000_000;
/// If we received more than REPORT_BANDWIDTH_THRESHOLD_COUNT` of messages from given peer it's bandwidth stats will be reported.
const REPORT_BANDWIDTH_THRESHOLD_COUNT: usize = 10_000;
/// How long a peer has to be unreachable, until we prune it from the in-memory graph.
const PRUNE_UNREACHABLE_PEERS_AFTER: time::Duration = time::Duration::hours(1);

/// Remove the edges that were created more that this duration ago.
const PRUNE_EDGES_AFTER: time::Duration = time::Duration::minutes(30);

/// If a peer is more than these blocks behind (comparing to our current head) - don't route any messages through it.
/// We are updating the list of unreliable peers every MONITOR_PEER_MAX_DURATION (60 seconds) - so the current
/// horizon value is roughly matching this threshold (if the node is 60 blocks behind, it will take it a while to recover).
/// If we set this horizon too low (for example 2 blocks) - we're risking excluding a lot of peers in case of a short
/// network issue.
const UNRELIABLE_PEER_HORIZON: u64 = 60;

/// Due to implementation limits of `Graph` in `near-network`, we support up to 128 client.
pub const MAX_NUM_PEERS: usize = 128;

/// Actor that manages peers connections.
pub struct PeerManagerActor {
    pub(crate) clock: time::Clock,
    /// Networking configuration.
    /// TODO(gprusak): this field is duplicated with
    /// NetworkState.config. Remove it from here.
    config: Arc<config::VerifiedConfig>,
    /// Peer information for this node.
    my_peer_id: PeerId,
    /// Flag that track whether we started attempts to establish outbound connections.
    started_connect_attempts: bool,
    /// Connected peers we have sent new edge update, but we haven't received response so far.
    local_peer_pending_update_nonce_request: HashMap<PeerId, u64>,

    pub(crate) state: Arc<NetworkState>,
}

/// TEST-ONLY
/// A generic set of events (observable in tests) that the Network may generate.
/// Ideally the tests should observe only public API properties, but until
/// we are at that stage, feel free to add any events that you need to observe.
/// In particular prefer emitting a new event to polling for a state change.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Event {
    ServerStarted,
    RoutedMessageDropped,
    RoutingTableUpdate { next_hops: Arc<routing::NextHopTable>, pruned_edges: Vec<Edge> },
    Ping(Ping),
    Pong(Pong),
    SetChainInfo,
    // Reported once a message has been processed.
    // In contrast to typical RPC protocols, many P2P messages do not trigger
    // sending a response at the end of processing.
    // However, for precise instrumentation in tests it is useful to know when
    // processing has been finished. We simulate the "RPC response" by reporting
    // an event MessageProcessed.
    //
    // Given that processing is asynchronous and unstructured as of now,
    // it is hard to pinpoint all the places when the processing of a message is
    // actually complete. Currently this event is reported only for some message types,
    // feel free to add support for more.
    MessageProcessed(tcp::Tier, PeerMessage),
    // Reported when a handshake has been started.
    HandshakeStarted(crate::peer::peer_actor::HandshakeStartedEvent),
    // Reported when a handshake has been successfully completed.
    HandshakeCompleted(crate::peer::peer_actor::HandshakeCompletedEvent),
    // Reported when the TCP connection has been closed.
    ConnectionClosed(crate::peer::peer_actor::ConnectionClosedEvent),
}

impl actix::Actor for PeerManagerActor {
    type Context = actix::Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Start server if address provided.
        if let Some(server_addr) = self.config.node_addr {
            tracing::debug!(target: "network", at = ?server_addr, "starting public server");
            let clock = self.clock.clone();
            let state = self.state.clone();
            ctx.spawn(wrap_future(async move {
                let mut listener = match tcp::Listener::bind(server_addr).await {
                    Ok(it) => it,
                    Err(e) => {
                        panic!("failed to start listening on server_addr={server_addr:?} e={e:?}")
                    }
                };
                state.config.event_sink.push(Event::ServerStarted);
                loop {
                    if let Ok(stream) = listener.accept().await {
                        // Always let the new peer to send a handshake message.
                        // Only then we can decide whether we should accept a connection.
                        // It is expected to be reasonably cheap: eventually, for TIER2 network
                        // we would like to exchange set of connected peers even without establishing
                        // a proper connection.
                        tracing::debug!(target: "network", from = ?stream.peer_addr, "got new connection");
                        if let Err(err) =
                            PeerActor::spawn(clock.clone(), stream, None, state.clone())
                        {
                            tracing::info!(target:"network", ?err, "PeerActor::spawn()");
                        }
                    }
                }
            }));
        }

        if let Some(cfg) = self.state.config.features.tier1.clone() {
            // Update TIER1 connections periodically.
            let clock = self.clock.clone();
            let state = self.state.clone();
            ctx.spawn(wrap_future(async move {
                let mut interval = tokio::time::interval(cfg.connect_interval.try_into().unwrap());
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    state.tier1_connect(&clock).await;
                    interval.tick().await;
                }
            }));
            // Update and broadcast the list of TIER1 proxies periodically.
            let clock = self.clock.clone();
            let state = self.state.clone();
            ctx.spawn(wrap_future(async move {
                let mut interval =
                    tokio::time::interval(cfg.advertise_proxies_interval.try_into().unwrap());
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    state.tier1_advertise_proxies(&clock).await;
                    interval.tick().await;
                }
            }));
        }

        // Periodically push network information to client.
        self.push_network_info_trigger(ctx, self.config.push_info_period);

        // Periodically starts peer monitoring.
        tracing::debug!(target: "network", max_period=?self.config.monitor_peers_max_period, "monitor_peers_trigger");
        self.monitor_peers_trigger(
            ctx,
            MONITOR_PEERS_INITIAL_DURATION,
            (MONITOR_PEERS_INITIAL_DURATION, self.config.monitor_peers_max_period),
        );

        let state = self.state.clone();
        ctx.spawn(wrap_future(async move {
            let mut interval =
                tokio::time::interval(UPDATE_ROUTING_TABLE_INTERVAL.try_into().unwrap());
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let _timer = metrics::PEER_MANAGER_TRIGGER_TIME
                    .with_label_values(&["update_routing_table"])
                    .start_timer();
                interval.tick().await;
                state
                    .update_routing_table(
                        self.clock.now().checked_sub(PRUNE_UNREACHABLE_PEERS_AFTER),
                        self.clock.now_utc().checked_sub(PRUNE_EDGES_AFTER),
                    )
                    .await;
            }
        }));

        // Periodically prints bandwidth stats for each peer.
        self.report_bandwidth_stats_trigger(ctx, REPORT_BANDWIDTH_STATS_TRIGGER_INTERVAL);
    }

    /// Try to gracefully disconnect from connected peers.
    fn stopping(&mut self, _ctx: &mut Self::Context) -> actix::Running {
        tracing::warn!("PeerManager: stopping");
        self.state.tier2.broadcast_message(Arc::new(PeerMessage::Disconnect));
        self.state.routing_table_addr.do_send(StopMsg {});
        actix::Running::Stop
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        actix::Arbiter::current().stop();
    }
}

impl PeerManagerActor {
    pub fn spawn(
        clock: time::Clock,
        store: Arc<dyn near_store::db::Database>,
        config: config::NetworkConfig,
        client: client::Client,
        genesis_id: GenesisId,
    ) -> anyhow::Result<actix::Addr<Self>> {
        let config = config.verify().context("config")?;
        let store = store::Store::from(store);
        let peer_store =
            peer_store::PeerStore::new(&clock, config.peer_store.clone(), store.clone())
                .map_err(|e| anyhow::Error::msg(e.to_string()))?;
        tracing::debug!(target: "network",
               len = peer_store.len(),
               boot_nodes = config.peer_store.boot_nodes.len(),
               banned = peer_store.count_banned(),
               "Found known peers");
        tracing::debug!(target: "network", blacklist = ?config.peer_store.blacklist, "Blacklist");

        let whitelist_nodes = {
            let mut v = vec![];
            for wn in &config.whitelist_nodes {
                v.push(wn.try_into()?);
            }
            v
        };
        let my_peer_id = config.node_id();
        let config = Arc::new(config);
        Ok(Self::start_in_arbiter(&actix::Arbiter::new().handle(), move |ctx| Self {
            my_peer_id: my_peer_id.clone(),
            config: config.clone(),
            started_connect_attempts: false,
            local_peer_pending_update_nonce_request: HashMap::new(),
            state: Arc::new(NetworkState::new(
                &clock,
                store.clone(),
                peer_store,
                config.clone(),
                genesis_id,
                client,
                ctx.address().recipient(),
                whitelist_nodes,
            )),
            clock,
        }))
    }

    /// Periodically prints bandwidth stats for each peer.
    fn report_bandwidth_stats_trigger(
        &mut self,
        ctx: &mut actix::Context<Self>,
        every: time::Duration,
    ) {
        let _timer = metrics::PEER_MANAGER_TRIGGER_TIME
            .with_label_values(&["report_bandwidth_stats"])
            .start_timer();
        let mut total_bandwidth_used_by_all_peers: usize = 0;
        let mut total_msg_received_count: usize = 0;
        for (peer_id, connected_peer) in &self.state.tier2.load().ready {
            let bandwidth_used =
                connected_peer.stats.received_bytes.swap(0, Ordering::Relaxed) as usize;
            let msg_received_count =
                connected_peer.stats.received_messages.swap(0, Ordering::Relaxed) as usize;
            if bandwidth_used > REPORT_BANDWIDTH_THRESHOLD_BYTES
                || msg_received_count > REPORT_BANDWIDTH_THRESHOLD_COUNT
            {
                tracing::debug!(target: "bandwidth",
                    ?peer_id,
                    bandwidth_used, msg_received_count, "Peer bandwidth exceeded threshold",
                );
            }
            total_bandwidth_used_by_all_peers += bandwidth_used;
            total_msg_received_count += msg_received_count;
        }

        tracing::info!(
            target: "bandwidth",
            total_bandwidth_used_by_all_peers,
            total_msg_received_count, "Bandwidth stats"
        );

        near_performance_metrics::actix::run_later(
            ctx,
            every.try_into().unwrap(),
            move |act, ctx| {
                act.report_bandwidth_stats_trigger(ctx, every);
            },
        );
    }

    /// Check if it is needed to create a new outbound connection.
    /// If the number of active connections is less than `ideal_connections_lo` or
    /// (the number of outgoing connections is less than `minimum_outbound_peers`
    ///     and the total connections is less than `max_num_peers`)
    fn is_outbound_bootstrap_needed(&self) -> bool {
        let tier2 = self.state.tier2.load();
        let total_connections = tier2.ready.len() + tier2.outbound_handshakes.len();
        let potential_outbound_connections =
            tier2.ready.values().filter(|peer| peer.peer_type == PeerType::Outbound).count()
                + tier2.outbound_handshakes.len();

        (total_connections < self.config.ideal_connections_lo as usize
            || (total_connections < self.state.max_num_peers.load() as usize
                && potential_outbound_connections < self.config.minimum_outbound_peers as usize))
            && !self.config.outbound_disabled
    }

    /// Returns peers close to the highest height
    fn highest_height_peers(&self) -> Vec<FullPeerInfo> {
        let infos: Vec<_> =
            self.state.tier2.load().ready.values().map(|p| p.full_peer_info()).collect();

        // This finds max height among peers, and returns one peer close to such height.
        let max_height = match infos.iter().map(|i| i.chain_info.height).max() {
            Some(height) => height,
            None => return vec![],
        };
        // Find all peers whose height is within `highest_peer_horizon` from max height peer(s).
        infos
            .into_iter()
            .filter(|i| {
                i.chain_info.height.saturating_add(self.config.highest_peer_horizon) >= max_height
            })
            .collect()
    }

    // Get peers that are potentially unreliable and we should avoid routing messages through them.
    // Currently we're picking the peers that are too much behind (in comparison to us).
    fn unreliable_peers(&self) -> HashSet<PeerId> {
        let my_height = self.state.chain_info.load().height;
        // Find all peers whose height is below `highest_peer_horizon` from max height peer(s).
        self.state
            .tier2
            .load()
            .ready
            .values()
            .filter(|p| {
                p.chain_height.load(Ordering::Relaxed).saturating_add(UNRELIABLE_PEER_HORIZON)
                    < my_height
            })
            .map(|p| p.peer_info.id.clone())
            .collect()
    }

    /// Check if the number of connections (excluding whitelisted ones) exceeds ideal_connections_hi.
    /// If so, constructs a safe set of peers and selects one random peer outside of that set
    /// and sends signal to stop connection to it gracefully.
    ///
    /// Safe set contruction process:
    /// 1. Add all whitelisted peers to the safe set.
    /// 2. If the number of outbound connections is less or equal than minimum_outbound_connections,
    ///    add all outbound connections to the safe set.
    /// 3. Find all peers who sent us a message within the last peer_recent_time_window,
    ///    and add them one by one to the safe_set (starting from earliest connection time)
    ///    until safe set has safe_set_size elements.
    fn maybe_stop_active_connection(&self) {
        let tier2 = self.state.tier2.load();
        let filter_peers = |predicate: &dyn Fn(&connection::Connection) -> bool| -> Vec<_> {
            tier2
                .ready
                .values()
                .filter(|peer| predicate(&*peer))
                .map(|peer| peer.peer_info.id.clone())
                .collect()
        };

        // Build safe set
        let mut safe_set = HashSet::new();

        // Add whitelisted nodes to the safe set.
        let whitelisted_peers = filter_peers(&|p| self.state.is_peer_whitelisted(&p.peer_info));
        safe_set.extend(whitelisted_peers);

        // If there is not enough non-whitelisted peers, return without disconnecting anyone.
        if tier2.ready.len() - safe_set.len() <= self.config.ideal_connections_hi as usize {
            return;
        }

        // If there is not enough outbound peers, add them to the safe set.
        let outbound_peers = filter_peers(&|p| p.peer_type == PeerType::Outbound);
        if outbound_peers.len() + tier2.outbound_handshakes.len()
            <= self.config.minimum_outbound_peers as usize
        {
            safe_set.extend(outbound_peers);
        }

        // If there is not enough archival peers, add them to the safe set.
        if self.config.archive {
            let archival_peers = filter_peers(&|p| p.initial_chain_info.archival);
            if archival_peers.len() <= self.config.archival_peer_connections_lower_bound as usize {
                safe_set.extend(archival_peers);
            }
        }

        // Find all recently active peers.
        let now = self.clock.now();
        let mut active_peers: Vec<Arc<connection::Connection>> = tier2
            .ready
            .values()
            .filter(|p| {
                now - p.last_time_received_message.load() < self.config.peer_recent_time_window
            })
            .cloned()
            .collect();

        // Sort by established time.
        active_peers.sort_by_key(|p| p.established_time);
        // Saturate safe set with recently active peers.
        let set_limit = self.config.safe_set_size as usize;
        for p in active_peers {
            if safe_set.len() >= set_limit {
                break;
            }
            safe_set.insert(p.peer_info.id.clone());
        }

        // Build valid candidate list to choose the peer to be removed. All peers outside the safe set.
        let candidates = tier2.ready.values().filter(|p| !safe_set.contains(&p.peer_info.id));
        if let Some(p) = candidates.choose(&mut rand::thread_rng()) {
            tracing::debug!(target: "network", id = ?p.peer_info.id,
                tier2_len = tier2.ready.len(),
                ideal_connections_hi = self.config.ideal_connections_hi,
                "Stop active connection"
            );
            p.stop(None);
        }
    }

    /// Periodically monitor list of peers and:
    ///  - request new peers from connected peers,
    ///  - bootstrap outbound connections from known peers,
    ///  - unban peers that have been banned for awhile,
    ///  - remove expired peers,
    ///
    /// # Arguments:
    /// - `interval` - Time between consequent runs.
    /// - `default_interval` - we will set `interval` to this value once, after first successful connection
    /// - `max_interval` - maximum value of interval
    /// NOTE: in the current implementation `interval` increases by 1% every time, and it will
    ///       reach value of `max_internal` eventually.
    fn monitor_peers_trigger(
        &mut self,
        ctx: &mut actix::Context<Self>,
        mut interval: time::Duration,
        (default_interval, max_interval): (time::Duration, time::Duration),
    ) {
        let _span = tracing::trace_span!(target: "network", "monitor_peers_trigger").entered();
        let _timer =
            metrics::PEER_MANAGER_TRIGGER_TIME.with_label_values(&["monitor_peers"]).start_timer();

        self.state.peer_store.unban(&self.clock);

        if self.is_outbound_bootstrap_needed() {
            let tier2 = self.state.tier2.load();
            if let Some(peer_info) = self.state.peer_store.unconnected_peer(|peer_state| {
                // Ignore connecting to ourself
                self.my_peer_id == peer_state.peer_info.id
                    || self.config.node_addr == peer_state.peer_info.addr
                    // Or to peers we are currently trying to connect to
                    || tier2.outbound_handshakes.contains(&peer_state.peer_info.id)
            }) {
                // Start monitor_peers_attempts from start after we discover the first healthy peer
                if !self.started_connect_attempts {
                    self.started_connect_attempts = true;
                    interval = default_interval;
                }

                ctx.spawn(wrap_future({
                    let state = self.state.clone();
                    let clock = self.clock.clone();
                    async move {
                        if let Err(err) = async {
                            let stream = tcp::Stream::connect(&peer_info, tcp::Tier::T2).await.context("tcp::Stream::connect()")?;
                            PeerActor::spawn_and_handshake(clock,stream,None,state.clone()).await.context("PeerActor::spawn()")?;
                            anyhow::Ok(())
                        }.await {
                            tracing::info!(target:"network", ?err, "failed to connect to {peer_info}");
                        }
                    }
                }));
            }
        }

        // If there are too many active connections try to remove some connections
        self.maybe_stop_active_connection();

        if let Err(err) = self.state.peer_store.remove_expired(&self.clock) {
            tracing::error!(target: "network", ?err, "Failed to remove expired peers");
        };

        // Find peers that are not reliable (too much behind) - and make sure that we're not routing messages through them.
        let unreliable_peers = self.unreliable_peers();
        metrics::PEER_UNRELIABLE.set(unreliable_peers.len() as i64);
        self.state.graph.write().set_unreliable_peers(unreliable_peers);

        let new_interval = min(max_interval, interval * EXPONENTIAL_BACKOFF_RATIO);

        near_performance_metrics::actix::run_later(
            ctx,
            interval.try_into().unwrap(),
            move |act, ctx| {
                act.monitor_peers_trigger(ctx, new_interval, (default_interval, max_interval));
            },
        );
    }

    /// Return whether the message is sent or not.
    fn send_message_to_account_or_peer_or_hash(
        &mut self,
        target: &AccountOrPeerIdOrHash,
        msg: RoutedMessageBody,
    ) -> bool {
        let target = match target {
            AccountOrPeerIdOrHash::AccountId(account_id) => {
                return self.state.send_message_to_account(&self.clock, account_id, msg);
            }
            AccountOrPeerIdOrHash::PeerId(it) => PeerIdOrHash::PeerId(it.clone()),
            AccountOrPeerIdOrHash::Hash(it) => PeerIdOrHash::Hash(it.clone()),
        };

        self.state.send_message_to_peer(
            &self.clock,
            tcp::Tier::T2,
            self.state.sign_message(&self.clock, RawRoutedMessage { target, body: msg }),
        )
    }

    pub(crate) fn get_network_info(&self) -> NetworkInfo {
        let tier2 = self.state.tier2.load();
        NetworkInfo {
            connected_peers: tier2
                .ready
                .values()
                .map(|cp| ConnectedPeerInfo {
                    full_peer_info: cp.full_peer_info(),
                    received_bytes_per_sec: cp.stats.received_bytes_per_sec.load(Ordering::Relaxed),
                    sent_bytes_per_sec: cp.stats.sent_bytes_per_sec.load(Ordering::Relaxed),
                    last_time_peer_requested: cp
                        .last_time_peer_requested
                        .load()
                        .unwrap_or(self.clock.now()),
                    last_time_received_message: cp.last_time_received_message.load(),
                    connection_established_time: cp.established_time,
                    peer_type: cp.peer_type,
                })
                .collect(),
            num_connected_peers: tier2.ready.len(),
            peer_max_count: self.state.max_num_peers.load(),
            highest_height_peers: self.highest_height_peers(),
            sent_bytes_per_sec: tier2
                .ready
                .values()
                .map(|x| x.stats.sent_bytes_per_sec.load(Ordering::Relaxed))
                .sum(),
            received_bytes_per_sec: tier2
                .ready
                .values()
                .map(|x| x.stats.received_bytes_per_sec.load(Ordering::Relaxed))
                .sum(),
            known_producers: self
                .state
                .routing_table_view
                .get_announce_accounts()
                .into_iter()
                .map(|announce_account| KnownProducer {
                    account_id: announce_account.account_id,
                    peer_id: announce_account.peer_id.clone(),
                    // TODO: fill in the address.
                    addr: None,
                    next_hops: self.state.routing_table_view.view_route(&announce_account.peer_id),
                })
                .collect(),
            tier1_accounts: self.state.accounts_data.load().data.values().cloned().collect(),
        }
    }

    fn push_network_info_trigger(&self, ctx: &mut actix::Context<Self>, interval: time::Duration) {
        let network_info = self.get_network_info();
        let _timer = metrics::PEER_MANAGER_TRIGGER_TIME
            .with_label_values(&["push_network_info"])
            .start_timer();
        // TODO(gprusak): just spawn a loop.
        let state = self.state.clone();
        ctx.spawn(wrap_future(async move { state.client.network_info(network_info).await }));

        near_performance_metrics::actix::run_later(
            ctx,
            interval.try_into().unwrap(),
            move |act, ctx| {
                act.push_network_info_trigger(ctx, interval);
            },
        );
    }

    #[perf]
    fn handle_msg_network_requests(
        &mut self,
        msg: NetworkRequests,
        _ctx: &mut actix::Context<Self>,
    ) -> NetworkResponses {
        let _span =
            tracing::trace_span!(target: "network", "handle_msg_network_requests").entered();
        let _d = delay_detector::DelayDetector::new(|| {
            format!("network request {}", msg.as_ref()).into()
        });
        metrics::REQUEST_COUNT_BY_TYPE_TOTAL.with_label_values(&[msg.as_ref()]).inc();
        match msg {
            NetworkRequests::Block { block } => {
                self.state.tier2.broadcast_message(Arc::new(PeerMessage::Block(block)));
                NetworkResponses::NoResponse
            }
            NetworkRequests::Approval { approval_message } => {
                self.state.send_message_to_account(
                    &self.clock,
                    &approval_message.target,
                    RoutedMessageBody::BlockApproval(approval_message.approval),
                );
                NetworkResponses::NoResponse
            }
            NetworkRequests::BlockRequest { hash, peer_id } => {
                if self.state.tier2.send_message(peer_id, Arc::new(PeerMessage::BlockRequest(hash)))
                {
                    NetworkResponses::NoResponse
                } else {
                    NetworkResponses::RouteNotFound
                }
            }
            NetworkRequests::BlockHeadersRequest { hashes, peer_id } => {
                if self
                    .state
                    .tier2
                    .send_message(peer_id, Arc::new(PeerMessage::BlockHeadersRequest(hashes)))
                {
                    NetworkResponses::NoResponse
                } else {
                    NetworkResponses::RouteNotFound
                }
            }
            NetworkRequests::StateRequestHeader { shard_id, sync_hash, target } => {
                if self.send_message_to_account_or_peer_or_hash(
                    &target,
                    RoutedMessageBody::StateRequestHeader(shard_id, sync_hash),
                ) {
                    NetworkResponses::NoResponse
                } else {
                    NetworkResponses::RouteNotFound
                }
            }
            NetworkRequests::StateRequestPart { shard_id, sync_hash, part_id, target } => {
                if self.send_message_to_account_or_peer_or_hash(
                    &target,
                    RoutedMessageBody::StateRequestPart(shard_id, sync_hash, part_id),
                ) {
                    NetworkResponses::NoResponse
                } else {
                    NetworkResponses::RouteNotFound
                }
            }
            NetworkRequests::StateResponse { route_back, response } => {
                let body = match response {
                    StateResponseInfo::V1(response) => RoutedMessageBody::StateResponse(response),
                    response @ StateResponseInfo::V2(_) => {
                        RoutedMessageBody::VersionedStateResponse(response)
                    }
                };
                if self.state.send_message_to_peer(
                    &self.clock,
                    tcp::Tier::T2,
                    self.state.sign_message(
                        &self.clock,
                        RawRoutedMessage { target: PeerIdOrHash::Hash(route_back), body },
                    ),
                ) {
                    NetworkResponses::NoResponse
                } else {
                    NetworkResponses::RouteNotFound
                }
            }
            NetworkRequests::BanPeer { peer_id, ban_reason } => {
                self.state.disconnect_and_ban(&self.clock, &peer_id, ban_reason);
                NetworkResponses::NoResponse
            }
            NetworkRequests::AnnounceAccount(announce_account) => {
                self.state.broadcast_accounts(vec![announce_account]);
                NetworkResponses::NoResponse
            }
            NetworkRequests::PartialEncodedChunkRequest { target, request, create_time } => {
                metrics::PARTIAL_ENCODED_CHUNK_REQUEST_DELAY
                    .observe((self.clock.now() - create_time.0).as_seconds_f64());
                let mut success = false;

                // Make two attempts to send the message. First following the preference of `prefer_peer`,
                // and if it fails, against the preference.
                for prefer_peer in &[target.prefer_peer, !target.prefer_peer] {
                    if !prefer_peer {
                        if let Some(account_id) = target.account_id.as_ref() {
                            if self.state.send_message_to_account(
                                &self.clock,
                                account_id,
                                RoutedMessageBody::PartialEncodedChunkRequest(request.clone()),
                            ) {
                                success = true;
                                break;
                            }
                        }
                    } else {
                        let mut matching_peers = vec![];
                        for (peer_id, peer) in &self.state.tier2.load().ready {
                            if (peer.initial_chain_info.archival || !target.only_archival)
                                && peer.chain_height.load(Ordering::Relaxed) >= target.min_height
                                && peer.initial_chain_info.tracked_shards.contains(&target.shard_id)
                            {
                                matching_peers.push(peer_id.clone());
                            }
                        }

                        if let Some(matching_peer) = matching_peers.iter().choose(&mut thread_rng())
                        {
                            if self.state.send_message_to_peer(
                                &self.clock,
                                tcp::Tier::T2,
                                self.state.sign_message(
                                    &self.clock,
                                    RawRoutedMessage {
                                        target: PeerIdOrHash::PeerId(matching_peer.clone()),
                                        body: RoutedMessageBody::PartialEncodedChunkRequest(
                                            request.clone(),
                                        ),
                                    },
                                ),
                            ) {
                                success = true;
                                break;
                            }
                        } else {
                            tracing::debug!(target: "network", chunk_hash=?request.chunk_hash, "Failed to find any matching peer for chunk");
                        }
                    }
                }

                if success {
                    NetworkResponses::NoResponse
                } else {
                    tracing::debug!(target: "network", chunk_hash=?request.chunk_hash, "Failed to find a route for chunk");
                    NetworkResponses::RouteNotFound
                }
            }
            NetworkRequests::PartialEncodedChunkResponse { route_back, response } => {
                if self.state.send_message_to_peer(
                    &self.clock,
                    tcp::Tier::T2,
                    self.state.sign_message(
                        &self.clock,
                        RawRoutedMessage {
                            target: PeerIdOrHash::Hash(route_back),
                            body: RoutedMessageBody::PartialEncodedChunkResponse(response),
                        },
                    ),
                ) {
                    NetworkResponses::NoResponse
                } else {
                    NetworkResponses::RouteNotFound
                }
            }
            NetworkRequests::PartialEncodedChunkMessage { account_id, partial_encoded_chunk } => {
                if self.state.send_message_to_account(
                    &self.clock,
                    &account_id,
                    RoutedMessageBody::VersionedPartialEncodedChunk(partial_encoded_chunk.into()),
                ) {
                    NetworkResponses::NoResponse
                } else {
                    NetworkResponses::RouteNotFound
                }
            }
            NetworkRequests::PartialEncodedChunkForward { account_id, forward } => {
                if self.state.send_message_to_account(
                    &self.clock,
                    &account_id,
                    RoutedMessageBody::PartialEncodedChunkForward(forward),
                ) {
                    NetworkResponses::NoResponse
                } else {
                    NetworkResponses::RouteNotFound
                }
            }
            NetworkRequests::ForwardTx(account_id, tx) => {
                if self.state.send_message_to_account(
                    &self.clock,
                    &account_id,
                    RoutedMessageBody::ForwardTx(tx),
                ) {
                    NetworkResponses::NoResponse
                } else {
                    NetworkResponses::RouteNotFound
                }
            }
            NetworkRequests::TxStatus(account_id, signer_account_id, tx_hash) => {
                if self.state.send_message_to_account(
                    &self.clock,
                    &account_id,
                    RoutedMessageBody::TxStatusRequest(signer_account_id, tx_hash),
                ) {
                    NetworkResponses::NoResponse
                } else {
                    NetworkResponses::RouteNotFound
                }
            }
            NetworkRequests::Challenge(challenge) => {
                // TODO(illia): smarter routing?
                self.state.tier2.broadcast_message(Arc::new(PeerMessage::Challenge(challenge)));
                NetworkResponses::NoResponse
            }
        }
    }

    #[perf]
    fn handle_msg_set_adv_options(&mut self, msg: crate::test_utils::SetAdvOptions) {
        if let Some(set_max_peers) = msg.set_max_peers {
            self.state.max_num_peers.store(set_max_peers as u32);
        }
    }

    fn handle_peer_manager_message(
        &mut self,
        msg: PeerManagerMessageRequest,
        ctx: &mut actix::Context<Self>,
    ) -> PeerManagerMessageResponse {
        let _span =
            tracing::trace_span!(target: "network", "handle_peer_manager_message").entered();
        match msg {
            PeerManagerMessageRequest::NetworkRequests(msg) => {
                PeerManagerMessageResponse::NetworkResponses(
                    self.handle_msg_network_requests(msg, ctx),
                )
            }
            // TEST-ONLY
            PeerManagerMessageRequest::OutboundTcpConnect(stream) => {
                let peer_addr = stream.peer_addr;
                if let Err(err) =
                    PeerActor::spawn(self.clock.clone(), stream, None, self.state.clone())
                {
                    tracing::info!(target:"network", ?err, ?peer_addr, "spawn_outbound()");
                }
                PeerManagerMessageResponse::OutboundTcpConnect
            }
            // TEST-ONLY
            PeerManagerMessageRequest::SetAdvOptions(msg) => {
                self.handle_msg_set_adv_options(msg);
                PeerManagerMessageResponse::SetAdvOptions
            }
            // TEST-ONLY
            PeerManagerMessageRequest::FetchRoutingTable => {
                PeerManagerMessageResponse::FetchRoutingTable(self.state.routing_table_view.info())
            }
            // TEST-ONLY
            PeerManagerMessageRequest::PingTo { nonce, target } => {
                self.state.send_ping(&self.clock, tcp::Tier::T2, nonce, target);
                PeerManagerMessageResponse::PingTo
            }
        }
    }

    fn handle_peer_to_manager_msg(&mut self, msg: PeerToManagerMsg) -> PeerToManagerMsgResp {
        match msg {
            PeerToManagerMsg::PeersRequest(msg) => {
                PeerToManagerMsgResp::PeersRequest(self.handle_msg_peers_request(msg))
            }
            PeerToManagerMsg::PeersResponse(msg) => {
                self.handle_msg_peers_response(msg);
                PeerToManagerMsgResp::Empty
            }
        }
    }
}

/// Fetches NetworkInfo, which contains a bunch of stats about the
/// P2P network state. Currently used only in tests.
/// TODO(gprusak): In prod, NetworkInfo is pushed periodically from PeerManagerActor to ClientActor.
/// It would be cleaner to replace the push loop in PeerManagerActor with a pull loop
/// in the ClientActor.
impl actix::Handler<GetNetworkInfo> for PeerManagerActor {
    type Result = NetworkInfo;
    fn handle(&mut self, _: GetNetworkInfo, _ctx: &mut Self::Context) -> NetworkInfo {
        let _timer = metrics::PEER_MANAGER_MESSAGES_TIME
            .with_label_values(&["GetNetworkInfo"])
            .start_timer();
        let _span =
            tracing::trace_span!(target: "network", "handle", handler = "GetNetworkInfo").entered();
        self.get_network_info()
    }
}

impl actix::Handler<SetChainInfo> for PeerManagerActor {
    type Result = ();
    fn handle(&mut self, info: SetChainInfo, _ctx: &mut Self::Context) {
        let _timer =
            metrics::PEER_MANAGER_MESSAGES_TIME.with_label_values(&["SetChainInfo"]).start_timer();
        let _span =
            tracing::trace_span!(target: "network", "handle", handler = "SetChainInfo").entered();
        let SetChainInfo(info) = info;
        let state = self.state.clone();
        // We set state.chain_info and call accounts_data.set_keys
        // synchronously, therefore, assuming actix in-order delivery,
        // there will be no race condition between subsequent SetChainInfo
        // calls.
        state.chain_info.store(Arc::new(info.clone()));

        // If tier1 is not enabled, we skip set_keys() call.
        // This way self.state.accounts_data is always empty, hence no data
        // will be collected or broadcasted.
        if state.config.features.tier1.is_none() {
            return;
        }
        // If the key set didn't change, early exit.
        if !state.accounts_data.set_keys(info.tier1_accounts.clone()) {
            return;
        }
        // The set of tier1 accounts has changed.
        // We might miss some data, so we start a full sync with the tier2 peers.
        state.tier2.broadcast_message(Arc::new(PeerMessage::SyncAccountsData(SyncAccountsData {
            incremental: false,
            requesting_full_sync: true,
            accounts_data: state.accounts_data.load().data.values().cloned().collect(),
        })));
        state.config.event_sink.push(Event::SetChainInfo);
    }
}

impl actix::Handler<PeerToManagerMsg> for PeerManagerActor {
    type Result = PeerToManagerMsgResp;
    fn handle(&mut self, msg: PeerToManagerMsg, _ctx: &mut Self::Context) -> Self::Result {
        let _timer =
            metrics::PEER_MANAGER_MESSAGES_TIME.with_label_values(&[(&msg).into()]).start_timer();
        let _span = tracing::trace_span!(target: "network", "handle", handler = "PeerToManagerMsg")
            .entered();
        self.handle_peer_to_manager_msg(msg)
    }
}

impl actix::Handler<PeerManagerMessageRequest> for PeerManagerActor {
    type Result = PeerManagerMessageResponse;
    fn handle(&mut self, msg: PeerManagerMessageRequest, ctx: &mut Self::Context) -> Self::Result {
        let _timer =
            metrics::PEER_MANAGER_MESSAGES_TIME.with_label_values(&[(&msg).into()]).start_timer();
        let _span = tracing::trace_span!(target: "network", "handle", handler = "PeerManagerMessageRequest").entered();
        self.handle_peer_manager_message(msg, ctx)
    }
}
