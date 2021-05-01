use anyhow::{Context, Result};
use bitcoin::BlockHash;
use bitcoincore_rpc::RpcApi;
use futures::StreamExt;
use serde_json::{de::from_str, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{tcp, TcpListener},
    sync::mpsc::{channel, error::TryRecvError, unbounded_channel, Receiver, UnboundedSender},
};

use std::{collections::hash_map::HashMap, convert::TryFrom, future::Future, thread};

use crate::{
    config::Config,
    daemon::rpc_connect,
    electrum::{Client, Rpc},
    signals,
};

fn spawn_blocking<F>(name: &'static str, f: F) -> thread::JoinHandle<()>
where
    F: 'static + Send + FnOnce() -> Result<()>,
{
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            if let Err(e) = f() {
                warn!("{} thread failed: {}", name, e);
            }
        })
        .expect("failed to spawn a thread")
}

fn spawn_async<F>(name: &'static str, f: F) -> tokio::task::JoinHandle<()>
where
    F: 'static + Send + Future<Output = Result<()>>,
{
    tokio::spawn(async move {
        if let Err(e) = f.await {
            warn!("{} task failed: {}", name, e);
        }
    })
}

struct Peer {
    client: Client,
    stream: tcp::OwnedWriteHalf,
}

impl Peer {
    fn new(stream: tcp::OwnedWriteHalf) -> Self {
        Self {
            client: Client::default(),
            stream,
        }
    }
}

fn tip_receiver(config: &Config) -> Result<Receiver<BlockHash>> {
    let (mut tip_tx, tip_rx) = channel(1);
    let rpc = rpc_connect(&config)?;

    let duration = u64::try_from(config.wait_duration.as_millis()).unwrap();

    use tokio::sync::mpsc::error::TrySendError;
    spawn_blocking("tip_loop", move || loop {
        // FIXME: this should probably call get_best_block_hash in a loop in case there are
        // multiple blocks.
        let tip = rpc.get_best_block_hash()?;
        match tip_tx.try_send(tip) {
            Ok(_) | Err(TrySendError::Full(_)) => (),
            Err(TrySendError::Closed(_)) => bail!("tip receiver disconnected"),
        }
        rpc.wait_for_new_block(duration)?;
    });
    Ok(tip_rx)
}

pub async fn run(config: &Config, mut rpc: Rpc) -> Result<()> {
    let listener = TcpListener::bind(config.electrum_rpc_addr).await?;
    let mut tip_rx = tip_receiver(&config)?.fuse();
    info!("serving Electrum RPC on {}", listener.local_addr()?);

    let (server_tx, server_rx) = unbounded_channel();
    let mut server_rx = server_rx.fuse();
    spawn_async("accept_loop", accept_loop(listener, server_tx)); // detach accepting task
    let mut signal_rx = signals::register().fuse();

    let mut peers = HashMap::<usize, Peer>::new();
    loop {
        futures::select! {
            sig = signal_rx.next() => {
                match sig.context("signal channel disconnected")? {
                    signals::Signal::Exit => break,
                    signals::Signal::Trigger => (),
                }
            }
            tip = tip_rx.next() => match tip {
                Some(_) => (), // sync and update
                None => break, // daemon is shutting down
            },
            event = server_rx.next() => {
                let event = event.context("server disconnected")?;
                handle(&rpc, &mut peers, event).await;
                loop {
                    let event = match server_rx.get_mut().try_recv() {
                        Ok(event) => event,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Closed) => bail!("server disconnected"),
                    };
                    handle(&rpc, &mut peers, event).await;
                }
            },
        }
        rpc.sync().context("rpc sync failed")?;
        for (peer_id, peer) in peers.iter_mut() {
            let notifications = rpc.update_client(&mut peer.client)?;
            send(*peer_id, peer, &notifications).await?;
        }
    }
    info!("stopping Electrum RPC server");
    Ok(())
}

struct Event {
    peer_id: usize,
    msg: Message,
}

enum Message {
    New(tcp::OwnedWriteHalf),
    Request(String),
    Done,
}

async fn handle(rpc: &Rpc, peers: &mut HashMap<usize, Peer>, event: Event) {
    match event.msg {
        Message::New(stream) => {
            debug!("{}: connected", event.peer_id);
            peers.insert(event.peer_id, Peer::new(stream));
        }
        Message::Request(line) => {
            let result = match peers.get_mut(&event.peer_id) {
                Some(peer) => handle_request(rpc, event.peer_id, peer, line).await,
                None => {
                    warn!("{}: unknown peer for {}", event.peer_id, line);
                    Ok(())
                }
            };
            if let Err(e) = result {
                error!("{}: {}", event.peer_id, e);
                if let Some(mut peer) = peers.remove(&event.peer_id) {
                    let _ = peer.stream.shutdown().await;
                }
            }
        }
        Message::Done => {
            debug!("{}: disconnected", event.peer_id);
            peers.remove(&event.peer_id);
        }
    }
}

async fn handle_request(rpc: &Rpc, peer_id: usize, peer: &mut Peer, line: String) -> Result<()> {
    let request: Value = from_str(&line).with_context(|| format!("invalid request: {}", line))?;
    let response: Value = rpc
        .handle_request(&mut peer.client, request)
        .with_context(|| format!("failed to handle request: {}", line))?;
    send(peer_id, peer, &[response]).await
}

async fn send(peer_id: usize, peer: &mut Peer, values: &[Value]) -> Result<()> {
    for value in values {
        let mut response = value.to_string();
        debug!("{}: send {}", peer_id, response);
        response += "\n";
        peer.stream
            .write_all(response.as_bytes())
            .await
            .with_context(|| format!("failed to send response: {}", response))?;
    }
    Ok(())
}

async fn accept_loop(mut listener: TcpListener, server_tx: UnboundedSender<Event>) -> Result<()> {
    let mut incoming = listener.incoming().enumerate();
    loop {
        let (peer_id, conn) = match incoming.next().await {
            Some((peer_id, conn)) => (peer_id, conn),
            None => break,
        };
        let stream = conn.context("failed to accept")?;
        let tx = server_tx.clone();
        spawn_async("recv_loop", async move {
            let (stream_reader, stream_writer) = stream.into_split();
            if tx
                .send(Event {
                    peer_id,
                    msg: Message::New(stream_writer),
                })
                .is_err()
            {
                bail!("main loop exited");
            }
            let result = recv_loop(peer_id, stream_reader, &tx).await;
            let _ = tx.send(Event {
                peer_id,
                msg: Message::Done,
            });
            result
        });
    }
    Ok(())
}

async fn recv_loop(
    peer_id: usize,
    stream_reader: tcp::OwnedReadHalf,
    server_tx: &UnboundedSender<Event>,
) -> Result<()> {
    let reader = BufReader::new(stream_reader);
    let mut lines = reader.lines();
    loop {
        let line = match lines.next().await {
            Some(line) => line,
            None => break,
        };
        let line = line.with_context(|| format!("{}: recv failed", peer_id))?;
        debug!("{}: recv {}", peer_id, line);
        let msg = Message::Request(line);
        if server_tx.send(Event { peer_id, msg }).is_err() {
            break;
        }
    }
    Ok(())
}
