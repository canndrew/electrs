#![recursion_limit = "1024"]
// remove this.
#![allow(unused_imports)]

pub use electrs_json_rpc_macro::{json_rpc_service, json_rpc_client};

use {
    std::{
        io, mem,
        collections::HashMap,
        convert::{TryFrom, TryInto},
        sync::{
            atomic, Arc, Weak,
            atomic::AtomicI64,
        },
        future::Future,
        pin::Pin,
    },
    tokio::{
        sync::{
            Mutex,
            mpsc,
            oneshot,
        },
        io::{
            AsyncRead, AsyncBufRead, AsyncWrite, AsyncBufReadExt, BufReader, AsyncWriteExt,
            WriteHalf,
        },
    },
    thiserror::Error,
    serde_json::{json, Value as JsonValue},
    async_trait::async_trait,
    futures::{
        stream, Stream, StreamExt, TryStreamExt, sink, Sink, FutureExt,
        stream::FuturesUnordered,
    },
    pin_utils::pin_mut,
    crate::{
        json_types::*,
        json_types::errors::*,
    },
};

pub mod json_types;

pub mod error_codes {
    pub const PARSE_ERROR: i16 = -32700;
    pub const INVALID_REQUEST: i16 = -32600;
    pub const METHOD_NOT_FOUND: i16 = -32601;
    pub const INVALID_PARAMS: i16 = -32602;
    pub const INTERNAL_ERROR: i16 = -32603;
}

pub struct JsonRpcMessageWriter<A> {
    writer: A,
}

impl<A> JsonRpcMessageWriter<A>
where
    A: AsyncWrite + Unpin,
{
    pub fn new(writer: A) -> JsonRpcMessageWriter<A> {
        JsonRpcMessageWriter { writer }
    }

    pub async fn write_message(&mut self, message: JsonRpcMessage) -> io::Result<()> {
        let json = JsonValue::from(message);
        let mut bytes = serde_json::to_vec(&json).unwrap();
        bytes.push(b'\n');
        match self.writer.write_all(&bytes).await {
            Ok(_) => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub fn into_sink(self) -> impl Sink<JsonRpcMessage, Error = io::Error> {
        sink::unfold(self, |mut message_writer, message| async move {
            message_writer.write_message(message).await?;
            Ok(message_writer)
        })
    }
}

pub struct JsonRpcMessageReader<A> {
    reader: A,
}

#[derive(Debug, Error)]
pub enum ReadMessageError {
    #[error("io error reading stream: {}", source)]
    Io {
        source: io::Error,
    },
    #[error("error derializing json: {}", source)]
    Deserialize {
        source: serde_json::Error,
    },
    #[error("error parsing json-rpc message: {}", source)]
    Parse {
        source: JsonRpcMessageFromJsonError,
    },
}

impl<A> JsonRpcMessageReader<A>
where
    A: AsyncBufRead + Unpin,
{
    pub fn new(reader: A) -> JsonRpcMessageReader<A> {
        JsonRpcMessageReader { reader }
    }

    pub async fn read_message(&mut self) -> Result<Option<JsonRpcMessage>, ReadMessageError> {
        let mut line = String::new();
        match self.reader.read_line(&mut line).await {
            Ok(0) => return Ok(None),
            Ok(_) => (),
            Err(source) => return Err(ReadMessageError::Io { source }),
        };
        let json: JsonValue = match serde_json::from_str(&line) {
            Ok(json) => json,
            Err(source) => return Err(ReadMessageError::Deserialize { source }),
        };
        let message = match JsonRpcMessage::try_from(json) {
            Ok(message) => message,
            Err(source) => return Err(ReadMessageError::Parse { source }),
        };
        Ok(Some(message))
    }

    pub fn into_stream(self) -> impl Stream<Item = Result<JsonRpcMessage, ReadMessageError>> {
        stream::unfold(self, |mut message_reader| async move {
            match message_reader.read_message().await {
                Ok(Some(message)) => Some((Ok(message), message_reader)),
                Ok(None) => None,
                Err(err) => Some((Err(err), message_reader)),
            }
        })
    }
}

pub struct JsonRpcClient<A> {
    message_writer: Weak<Mutex<JsonRpcMessageWriter<A>>>,
    response_map: Weak<Mutex<ResponseMap>>,
}

impl<A> JsonRpcClient<A>
where
    A: AsyncWrite + Unpin,
{
    pub async fn notify(
        &self,
        method: &str,
        params: Option<JsonRpcParams>,
    ) -> Result<(), SendMessageError> {
        let request = JsonRpcRequest {
            id: None,
            method: method.to_owned(),
            params,
        };
        let message = JsonRpcMessage::Request(JsonRpcRequests::Single(request));
        let message_writer = match self.message_writer.upgrade() {
            Some(message_writer) => message_writer,
            None => return Err(SendMessageError::ConnectionDropped),
        };
        let mut message_writer = message_writer.lock().await;
        match message_writer.write_message(message).await {
            Ok(()) => Ok(()),
            Err(source) => Err(SendMessageError::Io { source }),
        }
    }

    pub async fn call_method(
        &self,
        method: &str,
        params: Option<JsonRpcParams>,
    ) -> Result<Result<JsonValue, JsonRpcError>, SendMessageError> {
        let (result_sender, result_receiver) = oneshot::channel();
        let request = {
            let response_map = match self.response_map.upgrade() {
                Some(response_map) => response_map,
                None => return Err(SendMessageError::ConnectionDropped),
            };
            let mut response_map = response_map.lock().await;
            let id = {
                let next_id = response_map.next_id;
                response_map.next_id = response_map.next_id.wrapping_add(1);
                next_id
            };
            response_map.response_map.insert(id, result_sender);
            JsonRpcRequest {
                id: Some(JsonRpcId::Number(id)),
                method: method.to_owned(),
                params,
            }
        };
        let message = JsonRpcMessage::Request(JsonRpcRequests::Single(request));
        let result = {
            let message_writer = match self.message_writer.upgrade() {
                Some(message_writer) => message_writer,
                None => return Err(SendMessageError::ConnectionDropped),
            };
            let mut message_writer = message_writer.lock().await;
            message_writer.write_message(message).await
        };
        match result {
            Ok(()) => (),
            Err(source) => return Err(SendMessageError::Io { source }),
        }
        match result_receiver.await {
            Ok(result) => Ok(result),
            Err(_recv_error) => Err(SendMessageError::ConnectionDropped),
        }
    }

    pub fn batch<'a>(&'a self) -> JsonRpcClientBatchRequestBuilder<'a, A> {
        JsonRpcClientBatchRequestBuilder {
            json_rpc_client: self,
            notifications: Vec::new(),
            method_calls: Vec::new(),
        }
    }
}

pub struct JsonRpcClientBatchRequestBuilder<'a, A> {
    json_rpc_client: &'a JsonRpcClient<A>,
    notifications: Vec<(String, Option<JsonRpcParams>)>,
    method_calls: Vec<(String, Option<JsonRpcParams>)>,
}

impl<'a, A> JsonRpcClientBatchRequestBuilder<'a, A>
where
    A: AsyncWrite + Unpin,
{
    pub fn notify(&mut self, method: &str, params: Option<JsonRpcParams>) {
        self.notifications.push((method.to_owned(), params));
    }

    pub fn call_method(&mut self, method: &str, params: Option<JsonRpcParams>) {
        self.method_calls.push((method.to_owned(), params));
    }

    pub async fn send(self) -> Result<Vec<Result<JsonValue, JsonRpcError>>, SendMessageError> {
        let (result_sender, result_receiver) = oneshot::channel();
        let num_method_calls = self.method_calls.len();
        let mut id = {
            let response_map = match self.json_rpc_client.response_map.upgrade() {
                Some(response_map) => response_map,
                None => return Err(SendMessageError::ConnectionDropped),
            };
            let mut response_map = response_map.lock().await;
            let initial_id = {
                let next_id = response_map.next_id;
                response_map.next_id = response_map.next_id.wrapping_add(num_method_calls as i64);
                next_id
            };
            response_map.batch_response_map.insert(initial_id, result_sender);
            initial_id
        };
        let request = {
            let mut requests = Vec::with_capacity(self.notifications.len() + self.method_calls.len());
            for (method, params) in self.notifications {
                let request = JsonRpcRequest {
                    id: None,
                    method,
                    params,
                };
                requests.push(request);
            }
            for (method, params) in self.method_calls {
                let request = JsonRpcRequest {
                    id: Some(JsonRpcId::Number(id)),
                    method,
                    params,
                };
                id += 1;
                requests.push(request);
            }
            JsonRpcRequests::Batch(requests)
        };
        let message = JsonRpcMessage::Request(request);
        let result = {
            let message_writer = match self.json_rpc_client.message_writer.upgrade() {
                Some(message_writer) => message_writer,
                None => return Err(SendMessageError::ConnectionDropped),
            };
            let mut message_writer = message_writer.lock().await;
            message_writer.write_message(message).await
        };
        match result {
            Ok(()) => (),
            Err(source) => return Err(SendMessageError::Io { source }),
        }
        match result_receiver.await {
            Ok(result) => Ok(result),
            Err(_recv_error) => Err(SendMessageError::ConnectionDropped),
        }
    }
}

#[derive(Debug, Error)]
pub enum SendMessageError {
    #[error("io error sending request: {}", source)]
    Io {
        source: io::Error,
    },
    #[error("the connection to the server has been dropped")]
    ConnectionDropped,
}

pub enum HandleMethodError {
    MethodNotFound,
    InvalidParams {
        message: String,
        data: Option<JsonValue>,
    },
    InternalError {
        message: String,
        data: Option<JsonValue>,
    },
    ApplicationError(JsonRpcError),
}

impl HandleMethodError {
    fn into_json_rpc_error(self, method: &str) -> JsonRpcError {
        match self {
            HandleMethodError::MethodNotFound => {
                JsonRpcError {
                    code: error_codes::METHOD_NOT_FOUND,
                    message: format!("method '{}' not found", method),
                    data: None,
                }
            },
            HandleMethodError::InvalidParams { message, data } => {
                JsonRpcError {
                    code: error_codes::INVALID_PARAMS,
                    message: format!("invalid params: {}", message),
                    data,
                }
            },
            HandleMethodError::InternalError { message, data } => {
                JsonRpcError {
                    code: error_codes::INTERNAL_ERROR,
                    message,
                    data,
                }
            },
            HandleMethodError::ApplicationError(err) => err,
        }
    }
}

#[async_trait]
pub trait JsonRpcService {
    async fn handle_method<'s, 'm>(
        &'s self,
        method: &'m str,
        params: Option<JsonRpcParams>,
    ) -> Result<JsonValue, HandleMethodError>;

    async fn handle_notification<'s, 'm>(
        &'s self,
        method: &'m str,
        params: Option<JsonRpcParams>,
    );
}

struct ResponseMap {
    next_id: i64,
    response_map: HashMap<i64, oneshot::Sender<Result<JsonValue, JsonRpcError>>>,
    batch_response_map: HashMap<i64, oneshot::Sender<Vec<Result<JsonValue, JsonRpcError>>>>,
}

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error("error reading from underlying stream: {}", source)]
    Read {
        source: ReadMessageError,
    },
    #[error("error writing to underlying stream: {}", source)]
    Write {
        source: io::Error,
    },
}

fn handle_client<A, S>(
    connection: A,
    service: S,
    max_concurrent_requests_per_client: usize,
) -> (JsonRpcClient<WriteHalf<A>>, impl Future<Output = Result<(), ConnectionError>>)
where
    A: AsyncBufRead + AsyncWrite + Unpin,
    S: JsonRpcService + 'static,
{
    let (reader, writer) = tokio::io::split(connection);
    let reader = BufReader::new(reader);
    let message_reader = JsonRpcMessageReader::new(reader);
    let message_writer = Arc::new(Mutex::new(JsonRpcMessageWriter::new(writer)));
    let response_map = Arc::new(Mutex::new(ResponseMap {
        next_id: 0,
        response_map: HashMap::new(),
        batch_response_map: HashMap::new(),
    }));

    let json_rpc_client = JsonRpcClient {
        message_writer: Arc::downgrade(&message_writer),
        response_map: Arc::downgrade(&response_map),
    };
    let server_task = async move {
        let mut active_requests = FuturesUnordered::new();
        let message_stream = message_reader.into_stream().fuse();
        pin_mut!(message_stream);
        loop {
            let response_opt = if active_requests.len() < max_concurrent_requests_per_client {
                futures::select! {
                    response_opt = active_requests.select_next_some() => response_opt,
                    message_res = message_stream.select_next_some() => {
                        match message_res {
                            Ok(JsonRpcMessage::Response(responses)) => {
                                handle_incoming_response(
                                    responses,
                                    &response_map,
                                ).await;
                            },
                            Ok(JsonRpcMessage::Request(JsonRpcRequests::Single(request))) => {
                                let JsonRpcRequest { id, method, params } = request;
                                match id {
                                    Some(id) => {
                                        let service = &service;
                                        let future = async move {
                                            let result = {
                                                service.handle_method(&method, params).await
                                            };
                                            let response = JsonRpcResponse {
                                                id,
                                                result: result.map_err(|err| {
                                                    err.into_json_rpc_error(&method)
                                                }),
                                            };
                                            Some(JsonRpcResponses::Single(response))
                                        };
                                        active_requests.push(
                                            future
                                            .left_future()
                                            .left_future()
                                        );
                                    },
                                    None => {
                                        let service = &service;
                                        let future = async move {
                                            service.handle_notification(&method, params).await;
                                            None
                                        };
                                        active_requests.push(
                                            future
                                            .left_future()
                                            .right_future()
                                        );
                                    },
                                }
                            },
                            Ok(JsonRpcMessage::Request(JsonRpcRequests::Batch(requests))) => {
                                let num_method_calls = {
                                    let mut num_method_calls = 0;
                                    for request in &requests {
                                        match request.id {
                                            Some(_) => num_method_calls += 1,
                                            None => (),
                                        }
                                    }
                                    num_method_calls
                                };
                                let responses = {
                                    Arc::new(Mutex::new(Vec::with_capacity(num_method_calls)))
                                };
                                for request in requests {
                                    let JsonRpcRequest { id, method, params } = request;
                                    match id {
                                        Some(id) => {
                                            let responses = responses.clone();
                                            let service = &service;
                                            let future = async move {
                                                let result = {
                                                    service.handle_method(&method, params).await
                                                };
                                                let response = JsonRpcResponse {
                                                    id,
                                                    result: result.map_err(|err| {
                                                        err.into_json_rpc_error(&method)
                                                    }),
                                                };
                                                let mut responses = responses.lock().await;
                                                responses.push(response);
                                                if responses.len() == num_method_calls {
                                                    let responses = {
                                                        mem::replace(&mut *responses, Vec::new())
                                                    };
                                                    Some(JsonRpcResponses::Batch(responses))
                                                } else {
                                                    None
                                                }
                                            };
                                            active_requests.push(
                                                future
                                                .right_future()
                                                .left_future()
                                            );
                                        },
                                        None => {
                                            let service = &service;
                                            let future = async move {
                                                service
                                                .handle_notification(&method, params).await;

                                                None
                                            };
                                            active_requests.push(
                                                future
                                                .right_future()
                                                .right_future()
                                            );
                                        },
                                    }
                                }
                            },
                            Err(source) => break Err(ConnectionError::Read { source }),
                        }
                        continue;
                    },
                    complete => break Ok(()),
                }
            } else {
                active_requests.select_next_some().await
            };
            if let Some(response) = response_opt {
                let message = JsonRpcMessage::Response(response);
                let mut message_writer = message_writer.lock().await;
                match message_writer.write_message(message).await {
                    Ok(()) => (),
                    Err(source) => break Err(ConnectionError::Write { source }),
                }
            }
        }
    };
    (json_rpc_client, server_task)
}

async fn handle_incoming_response(
    responses: JsonRpcResponses,
    response_map: &Arc<Mutex<ResponseMap>>,
) {
    match responses {
        JsonRpcResponses::Single(response) => {
            let JsonRpcResponse { id, result } = response;
            let id = match id {
                JsonRpcId::Number(id) => id,
                _ => return,
            };
            let result_sender = {
                let mut response_map = response_map.lock().await;
                match response_map.response_map.remove(&id) {
                    Some(result_sender) => result_sender,
                    None => return,
                }
            };
            let _ignore_error = result_sender.send(result);
        },
        JsonRpcResponses::Batch(mut responses) => {
            responses.sort_by_key(|response| {
                match response.id {
                    JsonRpcId::Number(n) => Some(n),
                    _ => None,
                }
            });
            let initial_id = match responses.first() {
                Some(JsonRpcResponse { id: JsonRpcId::Number(initial_id), .. }) => initial_id,
                _ => return,
            };
            let result_sender = {
                let mut response_map = response_map.lock().await;
                match response_map.batch_response_map.remove(&initial_id) {
                    Some(result_sender) => result_sender,
                    None => return,
                }
            };
            let mut results = Vec::with_capacity(responses.len());
            for response in responses {
                let result = response.result;
                results.push(result);
            }
            let _ignore_error = result_sender.send(results);
        },
    }
}

/*
pub fn run<A, S, C>(
    client_stream: C,
    max_concurrent_clients: usize,
    max_concurrent_requests_per_client: usize,
    max_concurrent_batched_requests_per_client: usize,
) -> impl Stream<Item = (A, io::Result<()>)>
where
    C: Stream<Item = (JsonRpcConnection<A>, S)>,
    A: AsyncRead + AsyncWrite + Send + 'static,
    S: JsonRpcService + Send + Sync + 'static,
{
    client_stream
    .map(move |(connection, service)| async move {
        tokio::spawn(handle_client(
            connection,
            service,
            max_concurrent_requests_per_client,
            max_concurrent_batched_requests_per_client,
        )).await.unwrap()
    })
    .buffer_unordered(max_concurrent_clients)
}
*/

#[cfg(test)]
mod test {
    use super::*;

    use {
        std::{
            collections::HashMap,
            time::Duration,
        },
        rand::Rng,
        tokio::{
            net::{TcpStream, TcpListener},
            io::AsyncWriteExt,
        },
        futures::{StreamExt, TryFutureExt, TryStreamExt, stream::FuturesUnordered},
    };

    struct TestService;

    #[json_rpc_service]
    impl TestService {
        #[method = "delay"]
        async fn delay(millis: u64, reply: String) -> Result<String, JsonRpcError> {
            tokio::time::delay_for(Duration::from_millis(millis)).await;
            Ok(reply)
        }
    }

    #[tokio::test]
    async fn start_server() {
        const NUM_CLIENTS: usize = 1000;
        const MAX_REQUESTS_PER_CLIENT: usize = 100;

        let mut listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let server = {
            let listener_stream = listener.incoming();
            listener_stream
            .take(NUM_CLIENTS)
            .map(|client_res| {
                let client = client_res.expect("error accepting tcp connection");
                let client = BufReader::new(client);
                let (_client, task) = handle_client(client, TestService, MAX_REQUESTS_PER_CLIENT);
                task
            })
            .buffer_unordered(NUM_CLIENTS)
            .map(|task_res| task_res.expect("error running client task"))
            .collect::<()>()
        };

        let client_num = std::sync::atomic::AtomicUsize::new(0);
        let clients = FuturesUnordered::new();
        for _ in 0..NUM_CLIENTS {
            let client = async {
                let mut tcp_stream = TcpStream::connect(local_addr).await.unwrap();

                let mut rng = rand::thread_rng();
                let num_requests = rng.gen::<usize>() % MAX_REQUESTS_PER_CLIENT;
                let mut requests = HashMap::new();
                let mut buffer = Vec::new();
                for _ in 0..num_requests {
                    let delay = (rng.sample::<f64, _>(rand_distr::Exp1) * 1000.0f64) as u64;
                    let msg = format!("{}", rng.gen::<f64>());
                    let id = loop {
                        let id = rng.gen();
                        if requests.contains_key(&id) {
                            continue;
                        }
                        requests.insert(id, msg.clone());
                        break id;
                    };
                    let request = {
                        let request = JsonRpcRequest {
                            method: format!("delay"),
                            params: {
                                Some(JsonRpcParams::Array(vec![
                                    JsonValue::Number(delay.into()),
                                    JsonValue::String(msg),
                                ]))
                            },
                            id: Some(JsonRpcId::Number(id)),
                        };
                        JsonValue::from(request)
                    };
                    serde_json::to_writer(&mut buffer, &request).unwrap();
                    buffer.push(b'\n');
                    tcp_stream.write_all(buffer.as_slice()).await.unwrap();
                    buffer.clear();
                }

                AsyncWriteExt::shutdown(&mut tcp_stream).await.unwrap();
                let tcp_stream_reader = BufReader::new(tcp_stream);
                let mut lines = tcp_stream_reader.lines();
                loop {
                    let line = match lines.next().await {
                        Some(line) => line.unwrap(),
                        None => break,
                    };
                    let response_json: JsonValue = {
                        serde_json::from_slice(line.as_bytes()).unwrap()
                    };
                    let response = JsonRpcResponse::try_from(response_json).unwrap();
                    let id = match response.id {
                        JsonRpcId::Number(id) => id,
                        _ => panic!("invalid id"),
                    };
                    let msg = match response.result {
                        Ok(JsonValue::String(msg)) => msg,
                        _ => panic!("invalid response"),
                    };
                    match requests.remove(&id) {
                        Some(original_msg) => assert_eq!(original_msg, msg),
                        None => panic!("unknown id"),
                    };
                }
                assert!(requests.is_empty());
                let clients_finished = {
                    1 + client_num.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                };
                println!("finished {}/{} clients", clients_finished, NUM_CLIENTS);
            };
            clients.push(client);
        }
        let clients = clients.for_each(|()| async { () });

        let ((), ()) = futures::join!(server, clients);
    }
}
