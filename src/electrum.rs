use anyhow::{bail, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::hex::{FromHex, ToHex},
    BlockHash, Txid,
};
use electrs_json_rpc::{
    json_rpc_client,
    json_rpc_service,
    DropConnection,
    //HandleMethodError, DropConnection, JsonRpcService,
    //json_types::JsonRpcParams,
    //client::ClientSendNotificationError,
    //json_types::JsonRpcError,
};
use rayon::prelude::*;
use serde_derive::{Deserialize, Serialize};
use serde_json::{from_value, json, Value};
//use async_trait::async_trait;

use std::{collections::HashMap, convert::Infallible, iter::FromIterator};

use crate::{
    cache::Cache, daemon::Daemon, merkle::Proof, metrics::Histogram, status::Status,
    tracker::Tracker, types::ScriptHash,
};

const ELECTRS_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "1.4";
const BANNER: &str = "Welcome to the Electrum Rust Server!";

const UNKNOWN_FEE: isize = -1; // (allowed by Electrum protocol)

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
    tracker: Tracker,
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

    pub fn handle_request(&self, client: &mut Client, value: Value) -> Result<Value> {
        let Request {
            id,
            jsonrpc,
            method,
            params,
        } = from_value(value).context("invalid request")?;
        self.rpc_duration.observe_duration(&method, || {
            let result = match method.as_str() {
                "blockchain.scripthash.get_history" => {
                    self.scripthash_get_history(client, from_value(params)?)
                }
                "blockchain.scripthash.subscribe" => {
                    self.scripthash_subscribe(client, from_value(params)?)
                }
                "blockchain.transaction.broadcast" => {
                    self.transaction_broadcast(from_value(params)?)
                }
                "blockchain.transaction.get" => self.transaction_get(from_value(params)?),
                "blockchain.transaction.get_merkle" => {
                    self.transaction_get_merkle(from_value(params)?)
                }
                "server.banner" => Ok(json!(BANNER)),
                "server.donation_address" => Ok(Value::Null),
                "server.peers.subscribe" => Ok(json!([])),
                "blockchain.block.header" => self.block_header(from_value(params)?),
                "blockchain.block.headers" => self.block_headers(from_value(params)?),
                "blockchain.estimatefee" => self.estimate_fee(from_value(params)?),
                "blockchain.headers.subscribe" => self.headers_subscribe(client),
                "blockchain.relayfee" => self.relayfee(),
                "mempool.get_fee_histogram" => self.get_fee_histogram(),
                "server.ping" => Ok(Value::Null),
                //"server.version" => self.version(from_value(params)?),
                &_ => bail!("unknown method '{}' with {}", method, params,),
            };

            Ok(match result {
                Ok(value) => json!({"jsonrpc": jsonrpc, "id": id, "result": value}),
                Err(err) => {
                    let msg = format!("RPC failed: {:#}", err);
                    warn!("{}", msg);
                    let error = json!({"code": 1, "message": msg});
                    json!({"jsonrpc": jsonrpc, "id": id, "error": error})
                }
            })
        })
    }

    fn headers_subscribe(&self, client: &mut Client) -> Result<Value> {
        let chain = self.tracker.chain();
        client.tip = Some(chain.tip());
        let height = chain.height();
        let header = chain.get_block_header(height).unwrap();
        Ok(json!({"hex": serialize(header).to_hex(), "height": height}))
    }

    fn block_header(&self, (height,): (usize,)) -> Result<Value> {
        let chain = self.tracker.chain();
        let header = match chain.get_block_header(height) {
            None => bail!("no header at {}", height),
            Some(header) => header,
        };
        Ok(json!(serialize(header).to_hex()))
    }

    fn block_headers(&self, (start_height, count): (usize, usize)) -> Result<Value> {
        let chain = self.tracker.chain();
        let max_count = 2016usize;

        let count = std::cmp::min(
            std::cmp::min(count, max_count),
            chain.height() - start_height + 1,
        );
        let heights = start_height..(start_height + count);
        let hex_headers = String::from_iter(
            heights.map(|height| serialize(chain.get_block_header(height).unwrap()).to_hex()),
        );

        Ok(json!({"count": count, "hex": hex_headers, "max": max_count}))
    }

    fn estimate_fee(&self, (nblocks,): (u16,)) -> Result<Value> {
        Ok(self
            .daemon
            .estimate_fee(nblocks)?
            .map(|fee_rate| json!(fee_rate.as_btc()))
            .unwrap_or_else(|| json!(UNKNOWN_FEE)))
    }

    fn relayfee(&self) -> Result<Value> {
        Ok(json!(self.daemon.get_relay_fee()?.as_btc())) // [BTC/kB]
    }

    fn scripthash_get_history(
        &self,
        client: &Client,
        (scripthash,): (ScriptHash,),
    ) -> Result<Value> {
        let status = client
            .status
            .get(&scripthash)
            .context("no subscription for scripthash")?;
        Ok(json!(self
            .tracker
            .get_history(status)
            .collect::<Vec<Value>>()))
    }

    fn scripthash_subscribe(
        &self,
        client: &mut Client,
        (scripthash,): (ScriptHash,),
    ) -> Result<Value> {
        let mut status = Status::new(scripthash);
        self.tracker
            .update_status(&mut status, &self.daemon, &self.cache)?;
        let statushash = status.statushash();
        client.status.insert(scripthash, status); // skip if already exists
        Ok(json!(statushash))
    }

    fn transaction_broadcast(&self, (tx_hex,): (String,)) -> Result<Value> {
        let tx_bytes = Vec::from_hex(&tx_hex).context("non-hex transaction")?;
        let tx = deserialize(&tx_bytes).context("invalid transaction")?;
        let txid = self.daemon.broadcast(&tx)?;
        Ok(json!(txid))
    }

    fn transaction_get(&self, args: TxGetArgs) -> Result<Value> {
        let (txid, verbose) = args.into();
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

    fn transaction_get_merkle(&self, (txid, height): (Txid, usize)) -> Result<Value> {
        let chain = self.tracker.chain();
        let blockhash = match chain.get_block_hash(height) {
            None => bail!("missing block at {}", height),
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
            None => bail!("missing tx {} for merkle proof", txid),
            Some(position) => Ok(proof_to_value(&Proof::create(&txids, position))),
        }
    }

    fn get_fee_histogram(&self) -> Result<Value> {
        Ok(json!(self.tracker.fees_histogram()))
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

struct RpcService<'r> {
    rpc: &'r Rpc,
    //peer: Peer,
}

#[json_rpc_service]
impl<'r> RpcService<'r> {
    /*
    #[method = "blockchain.scripthash.get_history"]
    pub async fn scripthash_get_history(scripthash: ScriptHash)
        -> Result<Vec<Value>, JsonRpcError>
    {
        drop(scripthash);
        unimplemented!()
    }

    #[method = "blockchain.scripthash.subscribe"]
    pub async fn scripthash_subscribe(scripthash: ScriptHash)
        -> Result<ScriptHashStatus, JsonRpcError>
    {
        drop(scripthash);
        unimplemented!()
    }

    #[method = "blockchain.transaction.broadcast"]
    pub async fn transaction_broadcast(raw_tx: RawTx)
        -> Result<Txid, JsonRpcError>
    {
        drop(raw_tx);
        unimplemented!()
    }

    #[method = "blockchain.transaction.get"]
    pub async fn transaction_get(tx_hash: Txid)
        -> Result<RawTx, JsonRpcError>
    {
        drop(tx_hash);
        unimplemented!()
    }

    #[method = "blockchain.transaction.get"]
    pub async fn transaction_get_verbose(tx_hash: Txid, verbose: bool)
        -> Result<RawTx, JsonRpcError>
    {
        if !verbose {
            return RpcService::transaction_get(tx_hash).await;
        }
        drop(tx_hash);
        drop(verbose);
        unimplemented!()
    }

    #[method = "blockchain.transaction.get_merkle"]
    pub async fn transaction_get_merkle(tx_hash: Txid, height: u32)
        -> Result<MerkleBranch, JsonRpcError>
    {
        drop(tx_hash);
        drop(height);
        unimplemented!()
    }
    */

    #[method = "server.banner"]
    pub async fn banner() -> Result<&'static str, Infallible> {
        Ok(BANNER)
    }

    #[method = "server.donation_address"]
    pub async fn donation_address() -> Result<Value, Infallible> {
        Ok(Value::Null)
    }

    /*
    #[method = "server.peers.subscribe"]
    pub async fn peers_subscribe()
        -> Result<Vec<PeerInfo>, Infallible>
    {
        unimplemented!()
    }

    #[method = "blockchain.block.header"]
    pub async fn block_header(height: BlockHeight)
        -> Result<BlockHeader, JsonRpcError>
    {
        drop(height);
        unimplemented!()
    }

    #[method = "blockchain.block.header"]
    pub async fn block_header_checkpoint(height: BlockHeight, cp_height: BlockHeight)
        -> Result<Value, JsonRpcError>
    {
        drop(height);
        drop(cp_height);
        unimplemented!()
    }

    #[method = "blockchain.block.headers"]
    pub async fn block_headers(start_height: BlockHeight, count: BlockHeightOffset)
        -> Result<BlockHeaders, JsonRpcError>
    {
        drop(start_height);
        drop(count);
        unimplemented!()
    }

    #[method = "blockchain.block.headers"]
    pub async fn block_headers_checkpoint(start_height: BlockHeight, count: BlockHeightOffset, cp_height: BlockHeight)
        -> Result<BlockHeaders, JsonRpcError>
    {
        drop(start_height);
        drop(count);
        drop(cp_height);
        unimplemented!()
    }

    #[method = "blockchain.estimatefee"]
    pub async fn estimate_fee(number: BlockHeightOffset)
        -> Result<FeeRate, JsonRpcError>
    {
    }

    #[method = "blockchain.headers.subscribe"]
    #[method = "blockchain.relayfee"]
    #[method = "mempool.get_fee_histogram"]
    */
    #[method = "server.ping"]
    pub async fn ping() -> Result<Value, Infallible> {
        Ok(Value::Null)
    }

    #[method = "server.version"]
    pub async fn version_anonymous_client(&self) -> Result<(String, &'static str), DropConnection> {
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
        /*
        #[notification = "blockchain.scripthash.subscribe"]
        async fn update_scripthash_status(
            &mut self,
            script_hash: &ScriptHash,
            status: &ScriptHashStatus,
        ) -> Result<(), ClientSendNotificationError>;
        */
    }
}

/*
#[derive(Serialize)]
pub struct ScriptHashStatus {}

#[derive(Serialize, Deserialize)]
pub struct RawTx {}
*/

/*
pub struct MetricTrackingRpcService<'r> {
    inner: RpcService<'r>,
}

#[async_trait]
impl<'r> JsonRpcService for MetricTrackingRpcService<'r> {
    async fn handle_method<'s, 'm>(
        &'s self,
        method: &'m str,
        params: Option<JsonRpcParams>,
    ) -> Result<Value, HandleMethodError> {
        self.inner.rpc.rpc_duration.observe_duration(method, || {
            self.inner.handle_method(method, params).await
        })
    }

    async fn handle_notification<'s, 'm>(
        &'s self,
        method: &'m str,
        params: Option<JsonRpcParams>,
    ) {
        self.inner.rpc.rpc_duration.observe_duration(method, || {
            self.inner.handle_notification(method, params).await
        })
    }
}



use tokio::net::TcpStream;

struct Peer {
    client: Client,
    rpc_client: RpcClient<TcpStream>,
}
*/
