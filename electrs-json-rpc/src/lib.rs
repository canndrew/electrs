#![type_length_limit="1100000"]
#![recursion_limit = "1024"]

pub use {
    electrs_json_rpc_macro::{json_rpc_service, json_rpc_client},
    crate::session::JsonRpcSession,
    crate::service::{JsonRpcService, HandleMethodError},
};

use {
    std::{
        io, mem, fmt,
        collections::HashMap,
        convert::{TryFrom, TryInto},
        sync::{Arc, Weak},
    },
    tokio::{
        sync::{
            Mutex,
            oneshot,
        },
        io::{
            AsyncRead, AsyncBufRead, AsyncWrite, AsyncBufReadExt, BufReader, AsyncWriteExt,
        },
    },
    thiserror::Error,
    serde_json::{json, Value as JsonValue},
    async_trait::async_trait,
    futures::{
        stream, Stream, StreamExt, sink, Sink, FutureExt,
        stream::FuturesUnordered,
    },
    pin_utils::pin_mut,
    crate::{
        json_types::*,
        json_types::errors::*,
        message_io::{JsonRpcMessageWriter, JsonRpcMessageReader, ReadMessageError},
        client::JsonRpcClient,
        session::ResponseMap,
        service::NullService,
    },
};

pub mod json_types;
pub mod message_io;
pub mod client;
mod session;
mod service;

pub const JSON_RPC_VERSION: &'static str = "2.0";

pub mod error_codes {
    pub const PARSE_ERROR: i16 = -32700;
    pub const INVALID_REQUEST: i16 = -32600;
    pub const METHOD_NOT_FOUND: i16 = -32601;
    pub const INVALID_PARAMS: i16 = -32602;
    pub const INTERNAL_ERROR: i16 = -32603;
}

#[cfg(test)]
mod test;

