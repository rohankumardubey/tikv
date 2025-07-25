// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use collections::{HashMap, HashSet};
use concurrency_manager::ConcurrencyManager;
use encryption_export::DataKeyManager;
use engine_rocks::{RocksEngine, RocksSnapshot, util};
use engine_test::raft::RaftTestEngine;
use engine_traits::{Engines, MiscExt, Peekable};
use health_controller::HealthController;
use kvproto::{
    kvrpcpb::ApiVersion,
    metapb,
    raft_cmdpb::*,
    raft_serverpb::{self, RaftMessage},
};
use protobuf::Message;
use raft::{SnapshotStatus, eraftpb::MessageType};
use raftstore::{
    Result,
    coprocessor::{CoprocessorHost, config::SplitCheckConfigManager},
    errors::Error as RaftError,
    router::{LocalReadRouter, RaftStoreRouter, ReadContext, ServerRaftStoreRouter},
    store::{
        SnapManagerBuilder,
        config::RaftstoreConfigManager,
        fsm::{ApplyRouter, RaftBatchSystem, RaftRouter, store::StoreMeta},
        *,
    },
};
use resource_control::ResourceGroupManager;
use resource_metering::CollectorRegHandle;
use service::service_manager::GrpcServiceManager;
use tempfile::TempDir;
use test_pd_client::TestPdClient;
use tikv::{
    config::{ConfigController, Module},
    import::SstImporter,
    server::{MultiRaftServer, Result as ServerResult, raftkv::ReplicaReadLockChecker},
};
use tikv_util::{
    config::VersionTrack,
    sys::disk,
    time::ThreadReadId,
    worker::{Builder as WorkerBuilder, LazyWorker},
};

use super::*;
use crate::Config;

pub struct ChannelTransportCore {
    snap_paths: HashMap<u64, (SnapManager, TempDir)>,
    routers: HashMap<u64, SimulateTransport<ServerRaftStoreRouter<RocksEngine, RaftTestEngine>>>,
}

#[derive(Clone)]
pub struct ChannelTransport {
    core: Arc<Mutex<ChannelTransportCore>>,
}

impl ChannelTransport {
    pub fn new() -> ChannelTransport {
        ChannelTransport {
            core: Arc::new(Mutex::new(ChannelTransportCore {
                snap_paths: HashMap::default(),
                routers: HashMap::default(),
            })),
        }
    }
}

impl Default for ChannelTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for ChannelTransport {
    fn send(&mut self, msg: RaftMessage) -> Result<()> {
        let from_store = msg.get_from_peer().get_store_id();
        let to_store = msg.get_to_peer().get_store_id();
        let to_peer_id = msg.get_to_peer().get_id();
        let region_id = msg.get_region_id();
        let is_snapshot = msg.get_message().get_msg_type() == MessageType::MsgSnapshot;

        if is_snapshot {
            let snap = msg.get_message().get_snapshot();
            let key = SnapKey::from_snap(snap).unwrap();
            let from = match self.core.lock().unwrap().snap_paths.get(&from_store) {
                Some(p) => {
                    p.0.register(key.clone(), SnapEntry::Sending);
                    p.0.get_snapshot_for_sending(&key).unwrap()
                }
                None => return Err(box_err!("missing temp dir for store {}", from_store)),
            };
            let to = match self.core.lock().unwrap().snap_paths.get(&to_store) {
                Some(p) => {
                    p.0.register(key.clone(), SnapEntry::Receiving);
                    let data = msg.get_message().get_snapshot().get_data();
                    let mut snapshot_data = raft_serverpb::RaftSnapshotData::default();
                    snapshot_data.merge_from_bytes(data).unwrap();
                    p.0.get_snapshot_for_receiving(&key, snapshot_data.take_meta())
                        .unwrap()
                }
                None => return Err(box_err!("missing temp dir for store {}", to_store)),
            };

            defer!({
                let core = self.core.lock().unwrap();
                core.snap_paths[&from_store]
                    .0
                    .deregister(&key, &SnapEntry::Sending);
                core.snap_paths[&to_store]
                    .0
                    .deregister(&key, &SnapEntry::Receiving);
            });

            copy_snapshot(from, to)?;
        }

        let core = self.core.lock().unwrap();

        match core.routers.get(&to_store) {
            Some(h) => {
                h.send_raft_msg(msg)?;
                if is_snapshot {
                    // should report snapshot finish.
                    let _ = core.routers[&from_store].report_snapshot_status(
                        region_id,
                        to_peer_id,
                        SnapshotStatus::Finish,
                    );
                }
                Ok(())
            }
            _ => Err(box_err!("missing sender for store {}", to_store)),
        }
    }

    fn set_store_allowlist(&mut self, _allowlist: Vec<u64>) {
        unimplemented!();
    }

    fn need_flush(&self) -> bool {
        false
    }

    fn flush(&mut self) {}
}

type SimulateChannelTransport = SimulateTransport<ChannelTransport>;

pub struct NodeCluster {
    trans: ChannelTransport,
    pd_client: Arc<TestPdClient>,
    nodes: HashMap<u64, MultiRaftServer<TestPdClient, RocksEngine, RaftTestEngine>>,
    snap_mgrs: HashMap<u64, SnapManager>,
    cfg_controller: HashMap<u64, ConfigController>,
    simulate_trans: HashMap<u64, SimulateChannelTransport>,
    concurrency_managers: HashMap<u64, ConcurrencyManager>,
    importers: HashMap<u64, Arc<SstImporter<RocksEngine>>>,
    #[allow(clippy::type_complexity)]
    post_create_coprocessor_host: Option<Box<dyn Fn(u64, &mut CoprocessorHost<RocksEngine>)>>,
}

impl NodeCluster {
    pub fn new(pd_client: Arc<TestPdClient>) -> NodeCluster {
        NodeCluster {
            trans: ChannelTransport::new(),
            pd_client,
            nodes: HashMap::default(),
            snap_mgrs: HashMap::default(),
            cfg_controller: HashMap::default(),
            simulate_trans: HashMap::default(),
            concurrency_managers: HashMap::default(),
            importers: HashMap::default(),
            post_create_coprocessor_host: None,
        }
    }
}

impl NodeCluster {
    #[allow(dead_code)]
    pub fn get_node_router(
        &self,
        node_id: u64,
    ) -> SimulateTransport<ServerRaftStoreRouter<RocksEngine, RaftTestEngine>> {
        self.trans
            .core
            .lock()
            .unwrap()
            .routers
            .get(&node_id)
            .cloned()
            .unwrap()
    }

    // Set a function that will be invoked after creating each CoprocessorHost. The
    // first argument of `op` is the node_id.
    // Set this before invoking `run_node`.
    #[allow(clippy::type_complexity)]
    pub fn post_create_coprocessor_host(
        &mut self,
        op: Box<dyn Fn(u64, &mut CoprocessorHost<RocksEngine>)>,
    ) {
        self.post_create_coprocessor_host = Some(op)
    }

    pub fn get_node(
        &mut self,
        node_id: u64,
    ) -> Option<&mut MultiRaftServer<TestPdClient, RocksEngine, RaftTestEngine>> {
        self.nodes.get_mut(&node_id)
    }

    pub fn get_concurrency_manager(&self, node_id: u64) -> ConcurrencyManager {
        self.concurrency_managers.get(&node_id).unwrap().clone()
    }

    pub fn get_cfg_controller(&self, node_id: u64) -> Option<&ConfigController> {
        self.cfg_controller.get(&node_id)
    }

    pub fn get_importer(&self, node_id: u64) -> Option<Arc<SstImporter<RocksEngine>>> {
        self.importers.get(&node_id).cloned()
    }
}

impl Simulator for NodeCluster {
    fn run_node(
        &mut self,
        node_id: u64,
        cfg: Config,
        engines: Engines<RocksEngine, RaftTestEngine>,
        store_meta: Arc<Mutex<StoreMeta>>,
        key_manager: Option<Arc<DataKeyManager>>,
        router: RaftRouter<RocksEngine, RaftTestEngine>,
        system: RaftBatchSystem<RocksEngine, RaftTestEngine>,
        _resource_manager: &Option<Arc<ResourceGroupManager>>,
    ) -> ServerResult<u64> {
        assert!(node_id == 0 || !self.nodes.contains_key(&node_id));
        let pd_worker = LazyWorker::new("test-pd-worker");

        let simulate_trans = SimulateTransport::new(self.trans.clone());
        let mut raft_store = cfg.raft_store.clone();
        raft_store.optimize_for(false);
        raft_store
            .validate(
                cfg.coprocessor.region_split_size(),
                cfg.coprocessor.enable_region_bucket(),
                cfg.coprocessor.region_bucket_size,
                false,
            )
            .unwrap();
        let bg_worker = WorkerBuilder::new("background").thread_count(2).create();
        let store_config = Arc::new(VersionTrack::new(raft_store));
        let mut node = MultiRaftServer::new(
            system,
            &cfg.server,
            store_config.clone(),
            cfg.storage.api_version(),
            Arc::clone(&self.pd_client),
            Arc::default(),
            bg_worker.clone(),
            HealthController::new(),
            None,
        );

        let (snap_mgr, snap_mgr_path) = if node_id == 0
            || !self
                .trans
                .core
                .lock()
                .unwrap()
                .snap_paths
                .contains_key(&node_id)
        {
            let tmp = test_util::temp_dir("test_cluster", cfg.prefer_mem);
            let snap_mgr = SnapManagerBuilder::default()
                .max_write_bytes_per_sec(cfg.server.snap_io_max_bytes_per_sec.0 as i64)
                .max_total_size(cfg.server.snap_max_total_size.0)
                .concurrent_recv_snap_limit(cfg.server.concurrent_recv_snap_limit)
                .encryption_key_manager(key_manager)
                .max_per_file_size(cfg.raft_store.max_snapshot_file_raw_size.0)
                .enable_multi_snapshot_files(true)
                .enable_receive_tablet_snapshot(cfg.raft_store.enable_v2_compatible_learner)
                .min_ingest_snapshot_limit(cfg.server.snap_min_ingest_size)
                .build(tmp.path().to_str().unwrap());
            (snap_mgr, Some(tmp))
        } else {
            let trans = self.trans.core.lock().unwrap();
            let (snap_mgr, _) = &trans.snap_paths[&node_id];
            (snap_mgr.clone(), None)
        };

        self.snap_mgrs.insert(node_id, snap_mgr.clone());

        // Create coprocessor.
        let mut coprocessor_host = CoprocessorHost::new(router.clone(), cfg.coprocessor.clone());

        if let Some(f) = self.post_create_coprocessor_host.as_ref() {
            f(node_id, &mut coprocessor_host);
        }

        let cm = ConcurrencyManager::new_for_test(1.into());
        self.concurrency_managers.insert(node_id, cm.clone());
        ReplicaReadLockChecker::new(cm.clone()).register(&mut coprocessor_host);

        let importer = {
            let dir = Path::new(engines.kv.path()).join("import-sst");
            Arc::new(
                SstImporter::new(&cfg.import, dir, None, cfg.storage.api_version(), false).unwrap(),
            )
        };
        self.importers.insert(node_id, importer.clone());

        let local_reader = LocalReader::new(
            engines.kv.clone(),
            StoreMetaDelegate::new(store_meta.clone(), engines.kv.clone()),
            router.clone(),
            coprocessor_host.clone(),
        );
        let cfg_controller = ConfigController::new(cfg.tikv.clone());

        let split_check_runner = SplitCheckRunner::new(
            Some(store_meta.clone()),
            engines.kv.clone(),
            router.clone(),
            coprocessor_host.clone(),
        );
        let split_scheduler = bg_worker.start("test-split-check", split_check_runner);
        cfg_controller.register(
            Module::Coprocessor,
            Box::new(SplitCheckConfigManager(split_scheduler.clone())),
        );
        // Spawn a task to update the disk status periodically.
        {
            let data_dir = PathBuf::from(engines.kv.path());
            let rocks_engine = Arc::downgrade(engines.kv.as_inner());
            let snap_mgr = snap_mgr.clone();
            bg_worker.spawn_interval_task(std::time::Duration::from_millis(1000), move || {
                if let Some(rocks_engine) = rocks_engine.upgrade() {
                    let snap_size = snap_mgr.get_total_snap_size().unwrap();
                    let kv_size = util::get_engine_cfs_used_size(rocks_engine.as_ref())
                        .expect("get kv engine size");
                    let used_size = snap_size + kv_size;
                    let (capacity, available) = disk::get_disk_space_stats(&data_dir).unwrap();

                    disk::set_disk_capacity(capacity);
                    disk::set_disk_used_size(used_size);
                    disk::set_disk_available_size(std::cmp::min(available, capacity - used_size));
                }
            });
        }

        node.try_bootstrap_store(engines.clone())?;
        node.start(
            engines.clone(),
            simulate_trans.clone(),
            snap_mgr.clone(),
            pd_worker,
            store_meta,
            coprocessor_host,
            importer,
            split_scheduler,
            AutoSplitController::default(),
            cm,
            CollectorRegHandle::new_for_test(),
            None,
            DiskCheckRunner::dummy(),
            GrpcServiceManager::dummy(),
        )?;
        assert!(
            engines
                .kv
                .get_msg::<metapb::Region>(keys::PREPARE_BOOTSTRAP_KEY)
                .unwrap()
                .is_none()
        );
        assert!(node_id == 0 || node_id == node.id());

        let node_id = node.id();
        debug!(
            "node_id: {} tmp: {:?}",
            node_id,
            snap_mgr_path
                .as_ref()
                .map(|p| p.path().to_str().unwrap().to_owned())
        );

        cfg_controller.register(
            Module::Raftstore,
            Box::new(RaftstoreConfigManager::new(
                node.refresh_config_scheduler(),
                store_config,
            )),
        );

        if let Some(tmp) = snap_mgr_path {
            self.trans
                .core
                .lock()
                .unwrap()
                .snap_paths
                .insert(node_id, (snap_mgr, tmp));
        }

        let router = ServerRaftStoreRouter::new(router, local_reader);
        self.trans
            .core
            .lock()
            .unwrap()
            .routers
            .insert(node_id, SimulateTransport::new(router));
        self.nodes.insert(node_id, node);
        self.cfg_controller.insert(node_id, cfg_controller);
        self.simulate_trans.insert(node_id, simulate_trans);

        Ok(node_id)
    }

    fn get_snap_dir(&self, node_id: u64) -> String {
        self.trans.core.lock().unwrap().snap_paths[&node_id]
            .1
            .path()
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn get_snap_mgr(&self, node_id: u64) -> &SnapManager {
        self.snap_mgrs.get(&node_id).unwrap()
    }

    fn stop_node(&mut self, node_id: u64) {
        if let Some(mut node) = self.nodes.remove(&node_id) {
            node.stop();
        }
        self.trans
            .core
            .lock()
            .unwrap()
            .routers
            .remove(&node_id)
            .unwrap();
    }

    fn get_node_ids(&self) -> HashSet<u64> {
        self.nodes.keys().cloned().collect()
    }

    fn async_command_on_node_with_opts(
        &self,
        node_id: u64,
        request: RaftCmdRequest,
        cb: Callback<RocksSnapshot>,
        opts: RaftCmdExtraOpts,
    ) -> Result<()> {
        if !self
            .trans
            .core
            .lock()
            .unwrap()
            .routers
            .contains_key(&node_id)
        {
            return Err(box_err!("missing sender for store {}", node_id));
        }

        let router = self
            .trans
            .core
            .lock()
            .unwrap()
            .routers
            .get(&node_id)
            .cloned()
            .unwrap();
        router.send_command(request, cb, opts)
    }

    fn async_read(
        &mut self,
        node_id: u64,
        batch_id: Option<ThreadReadId>,
        request: RaftCmdRequest,
        cb: Callback<RocksSnapshot>,
    ) {
        if !self
            .trans
            .core
            .lock()
            .unwrap()
            .routers
            .contains_key(&node_id)
        {
            let mut resp = RaftCmdResponse::default();
            let e: RaftError = box_err!("missing sender for store {}", node_id);
            resp.mut_header().set_error(e.into());
            cb.invoke_with_response(resp);
            return;
        }
        let mut guard = self.trans.core.lock().unwrap();
        let router = guard.routers.get_mut(&node_id).unwrap();
        let read_ctx = ReadContext::new(batch_id, None);
        router.read(read_ctx, request, cb).unwrap();
    }

    fn send_raft_msg(&mut self, msg: raft_serverpb::RaftMessage) -> Result<()> {
        self.trans.send(msg)
    }

    fn add_send_filter(&mut self, node_id: u64, filter: Box<dyn Filter>) {
        self.simulate_trans
            .get_mut(&node_id)
            .unwrap()
            .add_filter(filter);
    }

    fn clear_send_filters(&mut self, node_id: u64) {
        self.simulate_trans
            .get_mut(&node_id)
            .unwrap()
            .clear_filters();
    }

    fn add_recv_filter(&mut self, node_id: u64, filter: Box<dyn Filter>) {
        let mut trans = self.trans.core.lock().unwrap();
        trans.routers.get_mut(&node_id).unwrap().add_filter(filter);
    }

    fn clear_recv_filters(&mut self, node_id: u64) {
        let mut trans = self.trans.core.lock().unwrap();
        trans.routers.get_mut(&node_id).unwrap().clear_filters();
    }

    fn get_router(&self, node_id: u64) -> Option<RaftRouter<RocksEngine, RaftTestEngine>> {
        self.nodes.get(&node_id).map(|node| node.get_router())
    }

    fn get_apply_router(&self, node_id: u64) -> Option<ApplyRouter<RocksEngine>> {
        self.nodes.get(&node_id).map(|node| node.get_apply_router())
    }
}

// Compare to server cluster, node cluster does not have server layer and
// storage layer.
pub fn new_node_cluster(id: u64, count: usize) -> Cluster<NodeCluster> {
    let pd_client = Arc::new(TestPdClient::new(id, false));
    let sim = Arc::new(RwLock::new(NodeCluster::new(Arc::clone(&pd_client))));
    Cluster::new(id, count, sim, pd_client, ApiVersion::V1)
}

// This cluster does not support batch split, we expect it to transfer the
// `BatchSplit` request to `split` request
pub fn new_incompatible_node_cluster(id: u64, count: usize) -> Cluster<NodeCluster> {
    let pd_client = Arc::new(TestPdClient::new(id, true));
    let sim = Arc::new(RwLock::new(NodeCluster::new(Arc::clone(&pd_client))));
    Cluster::new(id, count, sim, pd_client, ApiVersion::V1)
}
