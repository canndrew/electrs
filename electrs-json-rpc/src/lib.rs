pub use electrs_json_rpc_macro::{json_rpc_service, json_rpc_client};

use {
    std::{
        io,
        convert::{TryFrom, TryInto},
    },
    tokio::{
        sync::{
            Mutex, mpsc,
            mpsc::{Sender, Receiver},
        },
        io::{AsyncRead, AsyncWrite, AsyncBufReadExt, BufReader, AsyncWriteExt},
    },
    thiserror::Error,
    serde_json::{json, Value as JsonValue},
    async_trait::async_trait,
    futures::{stream, Stream, StreamExt, TryStreamExt},
};

pub mod json_types;
use json_types::*;

pub mod error_codes {
    pub const PARSE_ERROR: i16 = -32700;
    pub const INVALID_REQUEST: i16 = -32600;
    pub const METHOD_NOT_FOUND: i16 = -32601;
    pub const INVALID_PARAMS: i16 = -32602;
    pub const INTERNAL_ERROR: i16 = -32603;
}

pub struct JsonRpcConnection<A> {
    connection: A,
    request_receiver: Receiver<JsonRpcRequest>,
}

impl<A> JsonRpcConnection<A>
where
    A: AsyncRead + AsyncWrite + Send + 'static,
{
    pub fn from_io_stream(connection: A) -> (JsonRpcConnection<A>, JsonRpcClient) {
        let (request_sender, request_receiver) = mpsc::channel(1);
        let json_rpc_connection = JsonRpcConnection {
            connection,
            request_receiver,
        };
        let json_rpc_client = JsonRpcClient {
            request_sender: Mutex::new(request_sender),
        };
        (json_rpc_connection, json_rpc_client)
    }
}

pub struct JsonRpcClient {
    request_sender: Mutex<Sender<JsonRpcRequest>>,
}

// TODO: requests and batch requests/notifications
impl JsonRpcClient {
    pub async fn notify(
        &self,
        method: &str,
        params: Option<JsonRpcParams>,
    ) -> bool {
        let request = JsonRpcRequest {
            method: method.to_owned(),
            params,
            id: None,
        };
        let result = {
            let mut sender = self.request_sender.lock().await;
            sender.send(request).await
        };
        result.is_ok()
    }
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

async fn handle_client<A, S>(
    connection: JsonRpcConnection<A>,
    service: S,
    max_concurrent_requests_per_client: usize,
    max_concurrent_batched_requests_per_client: usize,
) -> (A, io::Result<()>)
where
    A: AsyncRead + AsyncWrite + Send + 'static,
    S: JsonRpcService + Send + Sync + 'static,
{
    let JsonRpcConnection { connection, request_receiver } = connection;
    let (reader, mut writer) = tokio::io::split(connection);
    let mut reader = BufReader::new(reader);
    let result = {
        let lines = (&mut reader).lines();
        let response_opts = {
            lines
            .map_ok(|line| async {
                let response = handle_request_line(
                    line,
                    &service,
                    max_concurrent_batched_requests_per_client,
                ).await;
                Ok(response.map(JsonRpcMessage::Response))
            })
            .try_buffer_unordered(max_concurrent_requests_per_client)
        };
        let request_opts = {
            request_receiver
            .map(|request| Ok(Some(JsonRpcMessage::Request(request))))
        };
        let mut messages = stream::select(response_opts, request_opts);
        loop {
            let message = match messages.try_next().await {
                Ok(Some(Some(message))) => message,
                Ok(Some(None)) => continue,
                Ok(None) => break Ok(()),
                Err(err) => break Err(err),
            };
            let json = JsonValue::from(message);
            let mut bytes = serde_json::to_vec(&json).unwrap();
            bytes.push(b'\n');
            match writer.write_all(&bytes).await {
                Ok(_) => (),
                Err(err) => break Err(err),
            };
        }
    };
    let connection = reader.into_inner().unsplit(writer);
    (connection, result)
}

async fn handle_request_line<S>(
    line: String,
    service: &S,
    max_concurrent_batched_requests_per_client: usize,
) -> Option<JsonRpcResponses>
where
    S: JsonRpcService + Send + 'static,
{
    let multi_request_json = match serde_json::from_str(&line) {
        Ok(multi_request_json) => multi_request_json,
        Err(err) => {
            let response = JsonRpcResponse {
                id: JsonRpcId::Null,
                result: Err(JsonRpcError {
                    code: error_codes::PARSE_ERROR,
                    message: format!("invalid json: {}", err),
                    data: None,
                }),
            };
            let responses = JsonRpcResponses::Single(response);
            return Some(responses);
        },
    };
    match multi_request_json {
        JsonValue::Object(_) => {
            let response_opt = handle_request(multi_request_json, service).await;
            response_opt.map(JsonRpcResponses::Single)
        },
        JsonValue::Array(requests) => {
            let responses = {
                stream::iter(requests)
                .map(|request| handle_request(request, service))
                .buffer_unordered(max_concurrent_batched_requests_per_client) // FIXME
                .filter_map(|response_opt| async { response_opt })
                .collect::<Vec<_>>()
                .await
            };
            if responses.is_empty() {
                None
            } else {
                Some(JsonRpcResponses::Batch(responses))
            }
        },
        _ => {
            let response = JsonRpcResponse {
                id: JsonRpcId::Null,
                result: Err(JsonRpcError {
                    code: error_codes::INVALID_REQUEST,
                    message: format!("request object must be an object or array"),
                    data: None,
                }),
            };
            let responses = JsonRpcResponses::Single(response);
            Some(responses)
        },
    }
}

async fn handle_request<S>(
    request_json: JsonValue,
    service: &S,
) -> Option<JsonRpcResponse>
where
    S: JsonRpcService + Send + 'static,
{
    let request = match JsonRpcRequest::try_from(request_json) {
        Ok(request) => request,
        Err(request_from_json_error) => {
            let response = request_from_json_error.into_error_response();
            return Some(response);
        },
    };
    let JsonRpcRequest { id, method, params } = request;
    match id {
        Some(id) => {
            let result = {
                service
                .handle_method(&method, params)
                .await
                .map_err(|err| err.into_json_rpc_error(&method))
            };
            let response = JsonRpcResponse { id, result };
            Some(response)
        },
        None => {
            service.handle_notification(&method, params).await;
            None
        },
    }
}

pub trait IntoJsonRpcError {
    fn into_json_rpc_error(self) -> JsonRpcError;
}

impl IntoJsonRpcError for JsonRpcError {
    fn into_json_rpc_error(self) -> JsonRpcError {
        self
    }
}


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
        futures::{StreamExt, stream::FuturesUnordered},
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
            let client_stream = {
                let listener_stream = listener.incoming();
                listener_stream
                .take(NUM_CLIENTS)
                .filter_map(|result| async {
                    match result {
                        Ok(client) => {
                            let (connection, _notifier) = JsonRpcConnection::from_io_stream(client);
                            Some((connection, TestService))
                        },
                        Err(err) => panic!("tcp accept failed: {}", err),
                    }
                })
            };

            run(client_stream, NUM_CLIENTS, MAX_REQUESTS_PER_CLIENT, MAX_REQUESTS_PER_CLIENT)
            .filter_map(|(_client, result)| async { result.err() })
            .map(|err| {
                panic!("got an error: {}", err)
            })
            .collect()
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
