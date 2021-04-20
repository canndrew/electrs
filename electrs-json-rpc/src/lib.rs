pub use electrs_json_rpc_macro::json_rpc_service;

use {
    std::io,
    tokio::{
        sync::{
            Mutex, mpsc,
            mpsc::{Sender, Receiver},
        },
        io::{AsyncRead, AsyncWrite, AsyncBufReadExt, BufReader, AsyncWriteExt},
    },
    serde_json::{json, Value as JsonValue},
    async_trait::async_trait,
    futures::{stream, Stream, StreamExt, TryStreamExt},
};

mod json_encoded_types;
use json_encoded_types::*;

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
        let lines = (&mut reader).split(b'\n');
        let response_opts = {
            lines
            .map_ok(|line_bytes| async {
                let response = handle_request_line(
                    line_bytes,
                    &service,
                    max_concurrent_batched_requests_per_client,
                ).await;
                Ok(response.map(JsonRpcMultiRequestOrResponse::Response))
            })
            .try_buffer_unordered(max_concurrent_requests_per_client)
        };
        let request_opts = {
            request_receiver
            .map(|request| Ok(Some(JsonRpcMultiRequestOrResponse::Request(request))))
        };
        let mut messages = stream::select(response_opts, request_opts);
        loop {
            let message = match messages.try_next().await {
                Ok(Some(Some(message))) => message,
                Ok(Some(None)) => continue,
                Ok(None) => break Ok(()),
                Err(err) => break Err(err),
            };
            let json = message.into_json();
            let bytes = serde_json::to_vec(&json).unwrap();
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
    line_bytes: Vec<u8>,
    service: &S,
    max_concurrent_batched_requests_per_client: usize,
) -> Option<JsonRpcMultiResponse>
where
    S: JsonRpcService + Send + 'static,
{
    let multi_request_json = match serde_json::from_slice(&line_bytes) {
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
            let responses = JsonRpcMultiResponse::Single(response);
            return Some(responses);
        },
    };
    match multi_request_json {
        JsonValue::Object(_) => {
            let response_opt = handle_request(multi_request_json, service).await;
            response_opt.map(JsonRpcMultiResponse::Single)
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
                Some(JsonRpcMultiResponse::Batch(responses))
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
            let responses = JsonRpcMultiResponse::Single(response);
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
    let request = match JsonRpcRequest::from_json(request_json) {
        Ok(request) => request,
        Err((id_opt, message)) => {
            let id = match id_opt {
                Some(id) => id,
                None => JsonRpcId::Null,
            };
            let response = JsonRpcResponse {
                id,
                result: Err(JsonRpcError {
                    code: error_codes::INVALID_REQUEST,
                    message,
                    data: None,
                }),
            };
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

    use pin_utils::pin_mut;
    use std::time::Duration;
    use tokio::net::{TcpStream, TcpListener};
    use tokio::io::{AsyncWriteExt, AsyncReadExt};
    use futures::{FutureExt, StreamExt};

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
        let mut listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let client_stream = {
            let listener_stream = listener.incoming();
            listener_stream
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
        let request = "{\"jsonrpc\":\"2.0\",\"method\":\"delay\",\"params\":[1000,\"hello\"],\"id\":45}\n";
        let make_request = async {
            let mut response = Vec::new();
            let mut tcp_stream = TcpStream::connect(local_addr).await.unwrap();
            tcp_stream.write_all(request.as_bytes()).await.unwrap();
            AsyncWriteExt::shutdown(&mut tcp_stream).await.unwrap();
            tcp_stream.read_to_end(&mut response).await.unwrap();
            let value: JsonValue = serde_json::from_slice(&response).unwrap();
            value
        }.fuse();
        let server = run(client_stream, 10, 10, 10);
        let server_errors = {
            server
            .filter_map(|(_client, result)| async { result.err() })
            .fuse()
        };
        pin_mut!(server_errors);
        pin_mut!(make_request);
        let response = futures::select! {
            err = server_errors.select_next_some() => panic!("got an error: {:?}", err),
            response = make_request => response,
        };
        match response {
            JsonValue::Object(mut map) => {
                let id = map.remove("id").unwrap();
                let jsonrpc = map.remove("jsonrpc").unwrap();
                let result = map.remove("result").unwrap();
                assert!(map.is_empty());
                match id {
                    JsonValue::Number(n) => assert_eq!(n.as_u64().unwrap(), 45),
                    _ => panic!("unexpected type for id"),
                };
                match jsonrpc {
                    JsonValue::String(s) => assert_eq!(s, "2.0"),
                    _ => panic!("unexpected value for jsonrpc field"),
                }
                match result {
                    JsonValue::String(message) => assert_eq!(message, "hello"),
                    _ => panic!("unexpected result: {:?}", result),
                }
            },
            _ => panic!("invalid response: {:?}", response),
        }
    }
}
