use anyhow::{Context, Result};
use bitcoin::BlockHash;
use bitcoincore_rpc::RpcApi;
//use crossbeam_channel::{bounded, select, unbounded, Receiver, Sender};
use electrs_json_rpc::JsonRpcSession;
use futures::{stream, StreamExt};
use tokio::{
    sync::{
        mpsc::{channel, Sender, Receiver},
        RwLock,
    },
    net::{TcpStream, TcpListener},
    signal::unix::{signal, SignalKind},
};
use rayon::prelude::*;

use std::{
    collections::hash_map::HashMap,
    convert::TryFrom,
    sync::Arc,
    thread,
};

use crate::{
    config::Config,
    daemon::rpc_connect,
    electrum::{Client, Rpc, RpcClient, RpcService, MetricTrackingRpcService},
};

fn spawn(
    name: &'static str,
    f: impl 'static + Send + FnOnce() -> Result<()>,
) -> thread::JoinHandle<()> {
    let builder = thread::Builder::new().name(name.to_owned());
    builder
        .spawn(move || {
            if let Err(e) = f() {
                warn!("{} thread failed: {}", name, e);
            }
        })
        .expect("failed to spawn a thread")
}

pub(crate) struct Peer {
    pub client: Arc<RwLock<Client>>,
    pub rpc_client: RpcClient<tokio::io::WriteHalf<TcpStream>>,
}

impl Peer {
    fn new(stream: TcpStream) -> Self {
        Self {
            client: Client::default(),
            stream,
        }
    }
}

fn tip_receiver(config: &Config) -> Result<Receiver<BlockHash>> {
    let (tip_tx, tip_rx) = channel(1);
    let rpc = rpc_connect(&config)?;

    let duration = u64::try_from(config.wait_duration.as_millis()).unwrap();

    use tokio::sync::mpsc::error::SendError;
    spawn("tip_loop", move || loop {
        // FIXME: this should probably call get_best_block_hash in a loop in case there are
        // multiple blocks.
        let tip = rpc.get_best_block_hash()?;
        match tip_tx.blocking_send(tip) {
            Ok(()) => (),
            Err(SendError(_)) => bail!("tip receiver disconnected"),
        }
        rpc.wait_for_new_block(duration)?;
    });
    Ok(tip_rx)
}

pub async fn run(config: &Config, rpc: Rpc) -> Result<()> {
    let listener = TcpListener::bind(config.electrum_rpc_addr).await?;
    let mut tip_rx = tip_receiver(&config)?.fuse();
    info!("serving Electrum RPC on {}", listener.local_addr()?);

    let rpc = Arc::new(rpc);
    let (peer_tx, peer_rx) = channel(1);
    let mut peer_rx = peer_rx.fuse();
    // FIXME: this should be configurable
    const MAX_CONCURRENT_REQUESTS_PER_CLIENT: usize = 10;
    tokio::spawn(
        accept_loop(listener, rpc.clone(), peer_tx, MAX_CONCURRENT_REQUESTS_PER_CLIENT)
    );
    let mut exit_signal = {
        let sigint = signal(SignalKind::interrupt()).context("registering SIGINT handler")?;
        let sigterm = signal(SignalKind::terminate()).context("registering SIGTERM handler")?;
        stream::select(sigint, sigterm)
    };
    let mut trigger_signal = {
        signal(SignalKind::user_defined1()).context("registering SIGUSR1 handler")?.fuse()
    };

    let mut peers = HashMap::<usize, Peer>::new();
    loop {
        futures::select! {
            () = trigger_signal.select_next_some() => (),
            () = exit_signal.select_next_some() => break,
            tip_opt = tip_rx.next() => {
                match tip_opt {
                    Some(_block_hash) => (), // sync and update
                    None => break, // daemon is shutting down
                }
            },
            peer_event_opt = peer_rx.next() => {
                let (peer_id, peer_opt) = peer_event_opt.context("server disconnected")?;
                match peer_opt {
                    Some(peer) => {
                        match peers.insert(peer_id, peer) {
                            None => (),
                            Some(_peer) => {
                                error!("{}: reusing peer id", peer_id);
                            },
                        }
                    },
                    None => match peers.remove(&peer_id) {
                        Some(_peer) => (),
                        None => {
                            error!("{}: removing unknown peer", peer_id);
                        },
                    },
                }
                continue;
            },
        }
        rpc.sync().context("rpc sync failed")?;
        for (peer_id, peer) in peers.iter_mut() {
            let result = rpc.update_peer(peer).await?;
            match result {
                Ok(()) => (),
                Err(err) => {
                    debug!("{}: failed to notify peer: {}", peer_id, err);
                },
            }
        }
    }
    info!("stopping Electrum RPC server");
    return Ok(());
}


async fn accept_loop(
    mut listener: TcpListener,
    rpc: Arc<Rpc>,
    mut peer_tx: Sender<(usize, Option<Peer>)>,
    max_concurrent_requests_per_client: usize,
) {
    let mut incoming = listener.incoming().enumerate();
    loop {
        let (peer_id, stream) = match incoming.next().await {
            None => break,
            Some((peer_id, stream_res)) => {
                let stream = match stream_res {
                    Ok(stream) => stream,
                    Err(err) => {
                        warn!("{}: failed to accept: {}", peer_id, err);
                        continue;
                    },
                };
                (peer_id, stream)
            },
        };
        let (json_rpc_session, json_rpc_client) = JsonRpcSession::new(stream);
        let rpc_client = RpcClient::from_inner(json_rpc_client);
        let client = Arc::new(RwLock::new(Client::default()));
        let peer = Peer {
            client: client.clone(),
            rpc_client,
        };
        let rpc = rpc.clone();
        let rpc_service = RpcService { rpc, client };
        peer_tx.send((peer_id, Some(peer))).await;
        let mut peer_tx = peer_tx.clone();
        tokio::spawn(async move {
            let result = recv_loop(peer_id, json_rpc_session, rpc_service, max_concurrent_requests_per_client).await;
            match result {
                Ok(()) => (),
                Err(err) => warn!("{}", err),
            }
            peer_tx.send((peer_id, None)).await;
        });
    }
}

async fn recv_loop(
    peer_id: usize,
    json_rpc_session: JsonRpcSession<TcpStream>,
    rpc_service: RpcService,
    max_concurrent_requests: usize,
) -> Result<()> {
    let metric_tracking_rpc_service = MetricTrackingRpcService { inner: rpc_service };
    json_rpc_session.serve(
        metric_tracking_rpc_service,
        max_concurrent_requests
    ).await.with_context(|| format!("{}: recv failed", peer_id))
}

