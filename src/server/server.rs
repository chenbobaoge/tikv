// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::{Arc, RwLock};
use std::sync::mpsc::Sender;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use grpc::{ChannelBuilder, EnvBuilder, Environment, Server as GrpcServer, ServerBuilder};
use kvproto::tikvpb_grpc::*;
use kvproto::debugpb_grpc::create_debug;

use util::worker::Worker;
use storage::Storage;
use raftstore::store::{CopFlowStatistics, Engines, Msg, SnapManager, SnapshotStatusMsg};

use super::{Config, Result};
use coprocessor::{CopRequestStatistics, CopSender, EndPointHost, EndPointTask, Result as CopResult};
use super::service::*;
use super::transport::{RaftStoreRouter, ServerTransport};
use super::resolve::StoreAddrResolver;
use super::snap::{Runner as SnapHandler, Task as SnapTask};
use super::raft_client::RaftClient;

const DEFAULT_COPROCESSOR_BATCH: usize = 256;
const MAX_GRPC_RECV_MSG_LEN: usize = 10 * 1024 * 1024;

pub struct Server<T: RaftStoreRouter + 'static, S: StoreAddrResolver + 'static> {
    env: Arc<Environment>,
    // Grpc server.
    grpc_server: GrpcServer,
    local_addr: SocketAddr,
    // Transport.
    trans: ServerTransport<T, S>,
    raft_router: T,
    // The kv storage.
    storage: Storage,
    // For handling coprocessor requests.
    end_point_worker: Worker<EndPointTask>,
    // For sending/receiving snapshots.
    snap_mgr: SnapManager,
    snap_worker: Worker<SnapTask>,
}

#[derive(Clone)]
pub struct CopReport<R: RaftStoreRouter + 'static> {
    router: R,
}

impl<R: RaftStoreRouter + 'static> CopReport<R> {
    pub fn new(r: R) -> CopReport<R> {
        CopReport { router: r.clone() }
    }
}

impl<R: RaftStoreRouter + 'static> CopSender for CopReport<R> {
    fn send(&self, stats: CopRequestStatistics) -> CopResult<()> {
        box_try!(self.router.try_send(Msg::CoprocessorStats {
            request_stats: stats as CopFlowStatistics,
        }));
        Ok(())
    }
}

#[derive(Clone)]
struct MockCopSender {}
impl MockCopSender {
    fn new() -> MockCopSender {
        MockCopSender {}
    }
}
impl CopSender for MockCopSender {
    fn send(&self, _stats: CopRequestStatistics) -> CopResult<()> {
        Ok(())
    }
}

impl<T: RaftStoreRouter, S: StoreAddrResolver + 'static> Server<T, S> {
    #[allow(too_many_arguments)]
    pub fn new(
        cfg: &Config,
        region_split_size: usize,
        storage: Storage,
        raft_router: T,
        snapshot_status_sender: Sender<SnapshotStatusMsg>,
        resolver: S,
        snap_mgr: SnapManager,
        debug_engines: Option<Engines>,
    ) -> Result<Server<T, S>> {
        let env = Arc::new(
            EnvBuilder::new()
                .cq_count(cfg.grpc_concurrency)
                .name_prefix(thd_name!("grpc-server"))
                .build(),
        );
        let raft_client = Arc::new(RwLock::new(RaftClient::new(env.clone(), cfg.clone())));
        let end_point_worker = Worker::new("end-point-worker");
        let snap_worker = Worker::new("snap-handler");

        let kv_service = KvService::new(
            storage.clone(),
            end_point_worker.scheduler(),
            raft_router.clone(),
            snap_worker.scheduler(),
        );
        let addr = try!(SocketAddr::from_str(&cfg.addr));
        info!("listening on {}", addr);
        let ip = format!("{}", addr.ip());
        let channel_args = ChannelBuilder::new(env.clone())
            .stream_initial_window_size(cfg.grpc_stream_initial_window_size.0 as usize)
            .max_concurrent_stream(cfg.grpc_concurrent_stream)
            .max_receive_message_len(MAX_GRPC_RECV_MSG_LEN)
            .max_send_message_len(region_split_size as usize * 4)
            .build_args();
        let grpc_server = {
            let mut sb = ServerBuilder::new(env.clone())
                .bind(ip, addr.port())
                .channel_args(channel_args)
                .register_service(create_tikv(kv_service));
            if let Some(engines) = debug_engines {
                sb = sb.register_service(create_debug(DebugService::new(engines)));
            }
            try!(sb.build())
        };

        let addr = {
            let (ref host, port) = grpc_server.bind_addrs()[0];
            SocketAddr::new(try!(IpAddr::from_str(host)), port as u16)
        };

        let trans = ServerTransport::new(
            raft_client,
            snap_worker.scheduler(),
            raft_router.clone(),
            snapshot_status_sender,
            resolver,
        );

        let svr = Server {
            env: env.clone(),
            grpc_server: grpc_server,
            local_addr: addr,
            trans: trans,
            raft_router: raft_router,
            storage: storage,
            end_point_worker: end_point_worker,
            snap_mgr: snap_mgr,
            snap_worker: snap_worker,
        };

        Ok(svr)
    }

    pub fn transport(&self) -> ServerTransport<T, S> {
        self.trans.clone()
    }

    pub fn start(&mut self, cfg: &Config) -> Result<()> {
        let end_point = EndPointHost::new(
            self.storage.get_engine(),
            self.end_point_worker.scheduler(),
            cfg,
            MockCopSender::new(),
        );
        box_try!(
            self.end_point_worker
                .start_batch(end_point, DEFAULT_COPROCESSOR_BATCH)
        );
        let snap_runner = SnapHandler::new(
            self.env.clone(),
            self.snap_mgr.clone(),
            self.raft_router.clone(),
        );
        box_try!(self.snap_worker.start(snap_runner));
        self.grpc_server.start();
        info!("TiKV is ready to serve");
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.end_point_worker.stop();
        self.snap_worker.stop();
        if let Err(e) = self.storage.stop() {
            error!("failed to stop store: {:?}", e);
        }
        self.grpc_server.shutdown();
        Ok(())
    }

    // Return listening address, this may only be used for outer test
    // to get the real address because we may use "127.0.0.1:0"
    // in test to avoid port conflict.
    pub fn listening_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::sync::{Arc, Mutex};
    use std::sync::mpsc::{self, Sender};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use super::super::{Config, Result};
    use super::super::transport::RaftStoreRouter;
    use super::super::resolve::{Callback as ResolveCallback, StoreAddrResolver};
    use storage::{Config as StorageConfig, Storage};
    use kvproto::raft_serverpb::RaftMessage;
    use raftstore::Result as RaftStoreResult;
    use raftstore::store::Msg as StoreMsg;
    use raftstore::store::transport::Transport;

    #[derive(Clone)]
    struct MockResolver {
        addr: Arc<Mutex<Option<SocketAddr>>>,
    }

    impl StoreAddrResolver for MockResolver {
        fn resolve(&self, _: u64, cb: ResolveCallback) -> Result<()> {
            cb.call_box((self.addr.lock().unwrap().ok_or(box_err!("not set")),));
            Ok(())
        }
    }

    #[derive(Clone)]
    struct TestRaftStoreRouter {
        tx: Sender<usize>,
        report_unreachable_count: Arc<AtomicUsize>,
    }

    impl TestRaftStoreRouter {
        fn new(tx: Sender<usize>) -> TestRaftStoreRouter {
            TestRaftStoreRouter {
                tx: tx,
                report_unreachable_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl RaftStoreRouter for TestRaftStoreRouter {
        fn send(&self, _: StoreMsg) -> RaftStoreResult<()> {
            self.tx.send(1).unwrap();
            Ok(())
        }

        fn try_send(&self, _: StoreMsg) -> RaftStoreResult<()> {
            self.tx.send(1).unwrap();
            Ok(())
        }

        fn report_unreachable(&self, _: u64, _: u64, _: u64) -> RaftStoreResult<()> {
            let count = self.report_unreachable_count.clone();
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn test_peer_resolve() {
        let mut cfg = Config::default();
        let storage_cfg = StorageConfig::default();
        cfg.addr = "127.0.0.1:0".to_owned();

        let mut storage = Storage::new(&storage_cfg).unwrap();
        storage.start(&storage_cfg).unwrap();

        let (tx, rx) = mpsc::channel();
        let router = TestRaftStoreRouter::new(tx);
        let report_unreachable_count = router.report_unreachable_count.clone();
        let (snapshot_status_sender, _) = mpsc::channel();

        let addr = Arc::new(Mutex::new(None));
        let mut server = Server::new(
            &cfg,
            1024,
            storage,
            router,
            snapshot_status_sender,
            MockResolver { addr: addr.clone() },
            SnapManager::new("", None),
            None,
        ).unwrap();
        *addr.lock().unwrap() = Some(server.listening_addr());

        server.start(&cfg).unwrap();

        let mut trans = server.transport();
        for i in 0..10 {
            if i % 2 == 1 {
                trans.report_unreachable(RaftMessage::new());
            }
            assert_eq!(report_unreachable_count.load(Ordering::SeqCst), (i + 1) / 2);
        }
        let mut msg = RaftMessage::new();
        msg.set_region_id(1);
        trans.send(msg).unwrap();
        trans.flush();
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
        server.stop().unwrap();
    }
}
