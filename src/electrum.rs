use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::hex::{FromHex, ToHex},
    BlockHash, Txid,
};
use electrs_json_rpc::{
    json_rpc_client,
    json_rpc_service,
    json_rpc_error_code,
    json_types::{JsonRpcParams, JsonRpcError, JsonRpcErrorCode},
    client::ClientSendNotificationError,
    DropConnection,
    HandleMethodError,
    JsonRpcService,
};
use rayon::prelude::*;
use serde_derive::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use std::{collections::HashMap, convert::Infallible, iter::FromIterator, sync::Arc};

use crate::{
    cache::Cache, daemon::Daemon, merkle::Proof, metrics::Histogram, status::Status,
    tracker::Tracker, types::{ScriptHash, StatusHash, BlockHeaderAtHeight, Height},
    server::Peer,
};

const ELECTRS_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "1.4";
const BANNER: &str = "Welcome to the Electrum Rust Server!";

const UNKNOWN_FEE: f64 = -1.0; // (allowed by Electrum protocol)

const BAD_REQUEST: JsonRpcErrorCode = json_rpc_error_code!(1);
const DAEMON_ERROR: JsonRpcErrorCode = json_rpc_error_code!(2);

pub enum ElectrumRpcError {
    BadRequest {
        message: String,
    },
    DaemonError {
        message: String,
    },
}

impl From<Infallible> for ElectrumRpcError {
    fn from(infallible: Infallible) -> ElectrumRpcError {
        match infallible {}
    }
}

impl From<anyhow::Error> for ElectrumRpcError {
    fn from(error: anyhow::Error) -> ElectrumRpcError {
        let message = error.to_string();
        if error.is::<bitcoincore_rpc::Error>() {
            ElectrumRpcError::DaemonError { message }
        } else {
            ElectrumRpcError::BadRequest { message }
        }
    }
}

impl From<ElectrumRpcError> for HandleMethodError {
    fn from(electrum_rpc_error: ElectrumRpcError) -> HandleMethodError {
        let json_rpc_error = match electrum_rpc_error {
            ElectrumRpcError::BadRequest { message } => JsonRpcError {
                code: BAD_REQUEST,
                message,
                data: None,
            },
            ElectrumRpcError::DaemonError { message } => JsonRpcError {
                code: DAEMON_ERROR,
                message,
                data: None,
            },
        };
        HandleMethodError::ApplicationError(json_rpc_error)
    }
}

/// Per-client Electrum protocol state
#[derive(Default)]
pub struct Client {
    tip: Option<BlockHash>,
    status: HashMap<ScriptHash, Status>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Request {
    id: Value,
    jsonrpc: String,
    method: String,

    #[serde(default)]
    params: Value,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[serde(untagged)]
enum Version {
    Single(String),
    Range(String, String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TxGetArgs {
    Txid((Txid,)),
    TxidVerbose(Txid, bool),
}

impl From<TxGetArgs> for (Txid, bool) {
    fn from(args: TxGetArgs) -> Self {
        match args {
            TxGetArgs::Txid((txid,)) => (txid, false),
            TxGetArgs::TxidVerbose(txid, verbose) => (txid, verbose),
        }
    }
}

/// Electrum RPC handler
pub struct Rpc {
    tracker: RwLock<Tracker>,
    cache: Cache,
    rpc_duration: Histogram,
    daemon: Daemon,
}

impl Rpc {
    pub fn new(tracker: Tracker, daemon: Daemon) -> Self {
        let rpc_duration = tracker.metrics().histogram_vec(
            "rpc_duration",
            "RPC duration (in seconds)",
            &["method"],
        );
        let cache = Cache::new();
        let tracker = RwLock::new(tracker);
        Self {
            tracker,
            cache,
            rpc_duration,
            daemon,
        }
    }

    pub fn sync(&mut self) -> Result<()> {
        self.tracker.sync(&self.daemon)
    }

    pub async fn update_peer(&self, peer: &mut Peer)
        -> Result<Result<(), ClientSendNotificationError>>
    {
        let tracker = self.tracker.read().await;
        let chain = tracker.chain();
        let mut client = peer.client.write().await;
        for (scripthash, status) in client.status.iter_mut() {
            if tracker
                .update_status(status, &self.daemon, &self.cache)
                .context("failed to update status")?
            {
                let result = {
                    peer
                    .rpc_client
                    .scripthash_subscribe(*scripthash, status.statushash())
                    .await
                };
                match result {
                    Ok(()) => (),
                    Err(err) => return Ok(Err(err)),
                }
            }
        }

        if let Some(old_tip) = client.tip {
            let new_tip = chain.tip();
            if old_tip != new_tip {
                client.tip = Some(new_tip);
                let height = chain.height();
                let header = chain.get_block_header(height).unwrap();

                let block_header_at_height = BlockHeaderAtHeight {
                    hex: serialize(header).to_hex(),
                    height,
                };
                let result = {
                    peer
                    .rpc_client
                    .headers_subscribe(block_header_at_height).await
                };
                match result {
                    Ok(()) => (),
                    Err(err) => return Ok(Err(err)),
                }
            }
        }
        Ok(Ok(()))
    }

    pub fn update_client(&self, client: &mut Client) -> Result<Vec<Value>> {
        let chain = self.tracker.chain();
        let mut notifications = client
            .status
            .par_iter_mut()
            .filter_map(|(scripthash, status)| -> Option<Result<Value>> {
                match self
                    .tracker
                    .update_status(status, &self.daemon, &self.cache)
                {
                    Ok(true) => Some(Ok(notification(
                        "blockchain.scripthash.subscribe",
                        &[json!(scripthash), json!(status.statushash())],
                    ))),
                    Ok(false) => None, // statushash is the same
                    Err(e) => Some(Err(e)),
                }
            })
            .collect::<Result<Vec<Value>>>()
            .context("failed to update status")?;

        if let Some(old_tip) = client.tip {
            let new_tip = self.tracker.chain().tip();
            if old_tip != new_tip {
                client.tip = Some(new_tip);
                let height = chain.height();
                let header = chain.get_block_header(height).unwrap();
                notifications.push(notification(
                    "blockchain.headers.subscribe",
                    &[json!({"hex": serialize(&header).to_hex(), "height": height})],
                ));
            }
        }
        Ok(notifications)
    }

    pub fn handle_request(&self, _client: &mut Client, _value: Value) -> Result<Value> {
        unimplemented!()
    }

    fn headers_subscribe(&self, client: &mut Client) -> BlockHeaderAtHeight {
        let chain = self.tracker.chain();
        client.tip = Some(chain.tip());
        let height = chain.height();
        let header = chain.get_block_header(height).unwrap();
        BlockHeaderAtHeight {
            hex: serialize(header).to_hex(),
            height,
        }
    }

    fn block_header(&self, height: Height) -> Result<String, ElectrumRpcError> {
        let chain = self.tracker.chain();
        let header = match chain.get_block_header(height) {
            None => Err(anyhow!("no header at {}", height))?,
            Some(header) => header,
        };
        Ok(serialize(header).to_hex())
    }

    fn block_headers(&self, start_height: Height, count: usize) -> Value {
        let chain = self.tracker.chain();
        let max_count = 2016usize;

        let count = std::cmp::min(
            std::cmp::min(count, max_count),
            (chain.height() + 1).saturating_sub(start_height),
        );
        let heights = start_height..(start_height + count);
        let hex_headers = String::from_iter(
            heights.map(|height| serialize(chain.get_block_header(height).unwrap()).to_hex()),
        );

        json!({"count": count, "hex": hex_headers, "max": max_count})
    }

    fn estimate_fee(&self, nblocks: u16) -> Result<f64, ElectrumRpcError> {
        Ok(self
            .daemon
            .estimate_fee(nblocks)?
            .map(|fee_rate| fee_rate.as_btc())
            .unwrap_or_else(|| UNKNOWN_FEE))
    }

    fn relayfee(&self) -> Result<f64, ElectrumRpcError> {
        Ok(self.daemon.get_relay_fee()?.as_btc()) // [BTC/kB]
    }

    fn scripthash_get_history(
        &self,
        client: &Client,
        scripthash: ScriptHash,
    ) -> Result<Vec<Value>, ElectrumRpcError> {
        let status = match client.status.get(&scripthash) {
            Some(status) => status,
            None => {
                return Err(ElectrumRpcError::BadRequest {
                    message: format!("no subscription for scripthash"),
                });
            },
        };
        Ok(self
            .tracker
            .get_history(status)
            .collect::<Vec<Value>>())
    }

    fn scripthash_subscribe(
        &self,
        client: &mut Client,
        scripthash: ScriptHash,
    ) -> Result<StatusHash, ElectrumRpcError> {
        let mut status = Status::new(scripthash);
        self.tracker
            .update_status(&mut status, &self.daemon, &self.cache)?;
        // TODO:
        // Better type-handling here? We know that the statushash is not None because we just
        // called sync via update_status.
        let statushash = status.statushash().unwrap();
        client.status.insert(scripthash, status); // skip if already exists
        Ok(statushash)
    }

    fn transaction_broadcast(&self, tx_hex: String) -> Result<Txid, ElectrumRpcError> {
        let tx_bytes = Vec::from_hex(&tx_hex).context("non-hex transaction")?;
        let tx = deserialize(&tx_bytes).context("invalid transaction")?;
        let txid = self.daemon.broadcast(&tx)?;
        Ok(txid)
    }

    fn transaction_get(&self, txid: Txid, verbose: bool) -> Result<Value, ElectrumRpcError> {
        if verbose {
            let blockhash = self.tracker.get_blockhash_by_txid(txid);
            return Ok(json!(self.daemon.get_transaction_info(&txid, blockhash)?));
        }
        let cached = self.cache.get_tx(&txid, |tx| serialize(tx).to_hex());
        Ok(match cached {
            Some(tx_hex) => json!(tx_hex),
            None => {
                debug!("tx cache miss: {}", txid);
                let blockhash = self.tracker.get_blockhash_by_txid(txid);
                json!(self.daemon.get_transaction_hex(&txid, blockhash)?)
            }
        })
    }

    // TODO: This could return a &Proof, no?
    fn transaction_get_merkle(&self, txid: Txid, height: Height) -> Result<Value, ElectrumRpcError> {
        let chain = self.tracker.chain();
        let blockhash = match chain.get_block_hash(height) {
            None => Err(anyhow!("missing block at {}", height))?,
            Some(blockhash) => blockhash,
        };
        let proof_to_value = |proof: &Proof| {
            json!({
                "block_height": height,
                "pos": proof.position(),
                "merkle": proof.to_hex(),
            })
        };
        if let Some(result) = self.cache.get_proof(blockhash, txid, proof_to_value) {
            return Ok(result);
        }
        debug!("txids cache miss: {}", blockhash);
        let txids = self.daemon.get_block_txids(blockhash)?;
        match txids.iter().position(|current_txid| *current_txid == txid) {
            None => Err(anyhow!("missing tx {} for merkle proof", txid))?,
            Some(position) => Ok(proof_to_value(&Proof::create(&txids, position))),
        }
    }

    fn get_fee_histogram(&self) -> &crate::mempool::Histogram {
        self.tracker.fees_histogram()
    }

    fn version(
        &self,
        client_id: String,
        client_version: Version,
    ) -> Result<(String, &'static str)> {
        match client_version {
            Version::Single(v) if v == PROTOCOL_VERSION => (),
            _ => {
                bail!(
                    "{} requested {:?}, server supports {}",
                    client_id,
                    client_version,
                    PROTOCOL_VERSION
                );
            }
        };
        let server_id = format!("electrs/{}", ELECTRS_VERSION);
        Ok((server_id, PROTOCOL_VERSION))
    }
}

fn notification(method: &str, params: &[Value]) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub struct RpcService {
    rpc: Arc<Rpc>,
    client: Arc<tokio::sync::RwLock<Client>>,
}

#[json_rpc_service]
impl RpcService {
    #[method = "blockchain.scripthash.get_history"]
    pub async fn scripthash_get_history(&self, scripthash: ScriptHash)
        -> Result<Vec<Value>, ElectrumRpcError>
    {
        let client = self.client.read().await;
        self.rpc.scripthash_get_history(&*client, scripthash)
    }

    #[method = "blockchain.scripthash.subscribe"]
    pub async fn scripthash_subscribe(&self, scripthash: ScriptHash)
        -> Result<StatusHash, ElectrumRpcError>
    {
        let mut client = self.client.write().await;
        self.rpc.scripthash_subscribe(&mut *client, scripthash)
    }

    #[method = "blockchain.transaction.broadcast"]
    pub async fn transaction_broadcast(&self, raw_tx: String)
        -> Result<Txid, ElectrumRpcError>
    {
        self.rpc.transaction_broadcast(raw_tx)
    }

    #[method = "blockchain.transaction.get"]
    pub async fn transaction_get(&self, tx_hash: Txid)
        -> Result<Value, ElectrumRpcError>
    {
        self.transaction_get_verbose(tx_hash, false).await
    }

    #[method = "blockchain.transaction.get"]
    pub async fn transaction_get_verbose(&self, tx_hash: Txid, verbose: bool)
        -> Result<Value, ElectrumRpcError>
    {
        self.rpc.transaction_get(tx_hash, verbose)
    }

    #[method = "blockchain.transaction.get_merkle"]
    pub async fn transaction_get_merkle(&self, tx_hash: Txid, height: Height)
        -> Result<Value, ElectrumRpcError>
    {
        self.rpc.transaction_get_merkle(tx_hash, height)
    }

    #[method = "server.banner"]
    pub async fn banner() -> Result<&'static str, Infallible> {
        Ok(BANNER)
    }

    #[method = "server.donation_address"]
    pub async fn donation_address() -> Result<Value, Infallible> {
        Ok(Value::Null)
    }

    #[method = "server.peers.subscribe"]
    pub async fn peers_subscribe()
        -> Result<Vec<Value>, Infallible>
    {
        Ok(Vec::new())
    }

    #[method = "blockchain.block.header"]
    pub async fn block_header(&self, height: Height)
        -> Result<String, ElectrumRpcError>
    {
        self.rpc.block_header(height)
    }

    #[method = "blockchain.block.header"]
    pub async fn block_header_checkpoint(&self, height: Height, cp_height: Height)
        -> Result<Value, ElectrumRpcError>
    {
        if cp_height != 0 {
            Err(anyhow!("cp_height argument not supported"))?;
        }
        let header = self.block_header(height).await?;
        Ok(Value::String(header))
    }

    #[method = "blockchain.block.headers"]
    pub async fn block_headers(&self, start_height: Height, count: usize)
        -> Result<Value, Infallible>
    {
        Ok(self.rpc.block_headers(start_height, count))
    }

    #[method = "blockchain.block.headers"]
    pub async fn block_headers_checkpoint(
        &self,
        start_height: Height,
        count: usize,
        cp_height: Height,
    ) -> Result<Value, ElectrumRpcError> {
        if cp_height != 0 {
            Err(anyhow!("cp_height argument not supported"))?;
        }
        Ok(self.block_headers(start_height, count).await?)
    }

    #[method = "blockchain.estimatefee"]
    pub async fn estimate_fee(&self, number: u16)
        -> Result<f64, ElectrumRpcError>
    {
        self.rpc.estimate_fee(number)
    }

    #[method = "blockchain.headers.subscribe"]
    pub async fn headers_subscribe(&self)
        -> Result<BlockHeaderAtHeight, Infallible>
    {
        let mut client = self.client.write().await;
        Ok(self.rpc.headers_subscribe(&mut *client))
    }

    #[method = "blockchain.relayfee"]
    pub async fn relayfee(&self) -> Result<f64, ElectrumRpcError> {
        self.rpc.relayfee()
    }

    #[method = "mempool.get_fee_histogram"]
    pub async fn get_fee_histogram(&self) -> Result<&crate::mempool::Histogram, Infallible> {
        Ok(self.rpc.get_fee_histogram())
    }

    #[method = "server.ping"]
    pub async fn ping() -> Result<Value, Infallible> {
        Ok(Value::Null)
    }

    #[method = "server.version"]
    pub async fn version_anonymous_client(&self)
        -> Result<(String, &'static str), DropConnection>
    {
        self.version_default_protocol(String::new()).await
    }

    #[method = "server.version"]
    pub async fn version_default_protocol(
        &self,
        client_name: String,
    ) -> Result<(String, &'static str), DropConnection> {
        self.version(client_name, Version::Single(String::from(PROTOCOL_VERSION)))
            .await
    }

    #[method = "server.version"]
    pub async fn version(
        &self,
        client_name: String,
        protocol_version: Version,
    ) -> Result<(String, &'static str), DropConnection> {
        match self.rpc.version(client_name, protocol_version) {
            Ok(version_info) => Ok(version_info),
            Err(err) => {
                // FIXME: this used to print the peer_id, as did all errors returned by
                // handle_request
                error!("{}", err);
                Err(DropConnection)
            }
        }
    }
}

json_rpc_client! {
    pub type RpcClient<T> {
        #[notification = "blockchain.scripthash.subscribe"]
        async fn scripthash_subscribe(
            &mut self,
            script_hash: ScriptHash,
            status: Option<StatusHash>,
        ) -> Result<(), ClientSendNotificationError>;

        #[notification = "blockchain.headers.subscribe"]
        async fn headers_subscribe(
            &mut self,
            header: BlockHeaderAtHeight,
        ) -> Result<(), ClientSendNotificationError>;
    }
}

/*
#[derive(Serialize)]
pub struct ScriptHashStatus {}

#[derive(Serialize, Deserialize)]
pub struct RawTx {}
*/

pub struct MetricTrackingRpcService {
    inner: RpcService,
}

#[async_trait]
impl JsonRpcService for MetricTrackingRpcService {
    async fn handle_method<'s, 'm>(
        &'s self,
        method: &'m str,
        params: Option<JsonRpcParams>,
    ) -> Result<Value, HandleMethodError> {
        let result = {
            self.inner
                .rpc
                .rpc_duration
                .observe_duration_async(method, self.inner.handle_method(method, params))
                .await
        };
        if let Err(err) = &result {
            warn!("RPC failed: {:#}", err);
        }
        result
    }

    async fn handle_notification<'s, 'm>(&'s self, method: &'m str, params: Option<JsonRpcParams>) {
        self.inner
            .rpc
            .rpc_duration
            .observe_duration_async(method, self.inner.handle_notification(method, params))
            .await
    }
}

