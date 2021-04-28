use anyhow::{Context, Result};
use bitcoin::BlockHash;
use bitcoincore_rpc::RpcApi;
//use crossbeam_channel::{bounded, select, unbounded, Receiver, Sender};
use tokio::{
    sync::mpsc::{channel, Sender, Receiver},
    net::{TcpStream, TcpListener},
};
use rayon::prelude::*;
use serde_json::{de::from_str, Value};

use std::{
    collections::hash_map::HashMap,
    convert::TryFrom,
    io::{BufRead, BufReader, Write},
    net::Shutdown,
    thread,
};

use crate::{
    config::Config,
    daemon::rpc_connect,
    electrum::{Client, Rpc},
    signals,
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

struct Peer {
    client: Arc<tokio::sync::RwLock<Client>>,
    rpc_client: RpcClient<TcpStream>,
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

pub async fn run(config: &Config, mut rpc: Rpc) -> Result<()> {
    let listener = TcpListener::bind(config.electrum_rpc_addr)?;
    let tip_rx = tip_receiver(&config)?;
    info!("serving Electrum RPC on {}", listener.local_addr()?);

    let (peer_tx, peer_rx) = channel(1);
    let accept_loop = tokio::spawn(async move || {
        accept_loop(listener, &rpc, peer_tx)
    });
    let exit_signal = {
        let sigint = signal(SignalKind::interrupt());
        let sigterm = signal(SignalKind::terminate());
        siging.merge(sigterm)
    };
    let trigger_signal = {
        signal(SignalKind::user_defined1())
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
            peer_opt = peer_rx.next() => {
                let (peer_id, peer) = peer_opt.context("server disconnected")?;
                peers.insert(peer_id, peer);
                continue;
            },
        }
        rpc.sync().context("rpc sync failed")?;
        peers
            .par_iter_mut()
            .map(|(peer_id, peer)| {
                let notifications = rpc.update_client(&mut peer.client)?;
                send(*peer_id, peer, &notifications)
            })
            .collect::<Result<_>>()?;
    }
    info!("stopping Electrum RPC server");
    return Ok(());
}

struct Event {
    peer_id: usize,
    msg: Message,
}

enum Message {
    New(TcpStream),
    Request(String),
    Done,
}

fn handle(rpc: &Rpc, peers: &mut HashMap<usize, Peer>, event: Event) {
    match event.msg {
        Message::New(stream) => {
            debug!("{}: connected", event.peer_id);
            peers.insert(event.peer_id, Peer::new(stream));
        }
        Message::Request(line) => {
            let result = match peers.get_mut(&event.peer_id) {
                Some(peer) => handle_request(rpc, event.peer_id, peer, line),
                None => {
                    warn!("{}: unknown peer for {}", event.peer_id, line);
                    Ok(())
                }
            };
            if let Err(e) = result {
                error!("{}: {}", event.peer_id, e);
                let _ = peers
                    .remove(&event.peer_id)
                    .map(|peer| peer.stream.shutdown(Shutdown::Both));
            }
        }
        Message::Done => {
            debug!("{}: disconnected", event.peer_id);
            peers.remove(&event.peer_id);
        }
    }
}

fn handle_request(rpc: &Rpc, peer_id: usize, peer: &mut Peer, line: String) -> Result<()> {
    let request: Value = from_str(&line).with_context(|| format!("invalid request: {}", line))?;
    let response: Value = rpc
        .handle_request(&mut peer.client, request)
        .with_context(|| format!("failed to handle request: {}", line))?;
    send(peer_id, peer, &[response])
}

fn send(peer_id: usize, peer: &mut Peer, values: &[Value]) -> Result<()> {
    for value in values {
        let mut response = value.to_string();
        debug!("{}: send {}", peer_id, response);
        response += "\n";
        peer.stream
            .write_all(response.as_bytes())
            .with_context(|| format!("failed to send response: {}", response))?;
    }
    Ok(())
}

fn accept_loop(
    listener: TcpListener,
    rpc: &Rpc,
    peer_tx: Sender<(usize, Peer)>,
) {
    let incoming = listener.incoming().enumerate();
    loop {
        let (peer_id, stream) = match incoming.next() {
            None => break,
            Some((peer_id, stream_res)) => {
                // FIXME: I'm pretty sure malicious clients can use this as a DOS vector by sending
                // tcp resets.
                let stream = stream_res.context("failed to accept")?;
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
        let rpc_service = RpcService { rpc, client };
        tokio::spawn(move || {
            let result = recv_loop(peer_id, json_rpc_session, rpc_service);
            match result {
                Ok(()) => (),
                Err(err) => warn!("{}", err),
            }
        });
        client_tx.send((peer_id, peer)).await;
    }
}

async fn recv_loop<'r>(
    peer_id: usize,
    json_rpc_session: JsonRpcSession<TcpStream>,
    client: RpcService<'r>,
) -> Result<()> {
    let rpc_service = RpcService { rpc, peer };
    let metric_tracking_rpc_service = MetricTrackingRpcService { inner: rpc_service };
    json_rpc_session.serve(
        metric_tracking_rpc_service,
        config.max_concurrent_requests_per_client
    ).with_context(|| format!("{}: recv failed", peer_id))
}

