pub use electrs_json_rpc_macro::json_rpc_service;

use std::io;
use tokio::io::{AsyncRead, AsyncWrite, AsyncBufReadExt, BufReader, AsyncWriteExt};
use serde_json::{json, Value as JsonValue};
use async_trait::async_trait;
use futures::{stream, Stream, StreamExt, TryStreamExt};

mod json_encoded_types;
use json_encoded_types::*;

pub mod error_codes {
    pub const PARSE_ERROR: i16 = -32700;
    pub const INVALID_REQUEST: i16 = -32600;
    pub const METHOD_NOT_FOUND: i16 = -32601;
    pub const INVALID_PARAMS: i16 = -32602;
}

pub enum HandleMethodError {
    MethodNotFound,
    InvalidParams {
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
            HandleMethodError::ApplicationError(err) => err,
        }
    }
}

#[async_trait]
pub trait JsonRpcService {
    async fn handle_method<'m>(
        &self,
        method: &'m str,
        params: Option<JsonRpcParams>,
    ) -> Result<JsonValue, HandleMethodError>;

    async fn handle_notification<'m>(
        &self,
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
    C: Stream<Item = (A, S)>,
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
    connection: A,
    service: S,
    max_concurrent_requests_per_client: usize,
    max_concurrent_batched_requests_per_client: usize,
) -> (A, io::Result<()>)
where
    A: AsyncRead + AsyncWrite + Send + 'static,
    S: JsonRpcService + Send + Sync + 'static,
{
    let (reader, mut writer) = tokio::io::split(connection);
    let mut reader = BufReader::new(reader);
    let result = {
        //let lines = SplitStream::new((&mut reader).split(b'\n'));
        let lines = (&mut reader).split(b'\n');
        let mut response_opts = {
            lines
            .map_ok(|line_bytes| async {
                Ok(handle_request_line(
                    line_bytes,
                    &service,
                    max_concurrent_batched_requests_per_client,
                ).await)
            })
            .try_buffer_unordered(max_concurrent_requests_per_client)
        };
        loop {
            let response = match response_opts.try_next().await {
                Ok(Some(Some(response))) => response,
                Ok(Some(None)) => continue,
                Ok(None) => break Ok(()),
                Err(err) => break Err(err),
            };
            let json = response.into_json();
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

#[cfg(test)]
mod test {
    use super::*;

    use pin_utils::pin_mut;
    use std::time::Duration;
    use tokio::net::{TcpStream, TcpListener};
    use tokio::io::AsyncReadExt;
    use futures::{FutureExt, StreamExt};

    struct TestService;

    #[json_rpc_service]
    impl TestService {
        #[method = "delay"]
        async fn delay(millis: u32, reply: &str) {
            reply
        }
    }

    /*
    #[async_trait]
    impl JsonRpcService for TestService {
        async fn handle_method<'m>(
            &self, 
            method: &'m str,
            params: Option<JsonRpcParams>,
        ) -> Result<JsonValue, HandleMethodError> {
            match method {
                "delay" => {
                    let params = {
                        params
                        .ok_or_else(|| HandleMethodError::InvalidParams {
                            message: format!("missing params"),
                            data: None,
                        })?
                    };
                    match params {
                        JsonRpcParams::Array(mut values) => {
                            let message_json = {
                                values
                                .pop()
                                .ok_or_else(|| HandleMethodError::InvalidParams {
                                    message: format!("missing message parameter"),
                                    data: None,
                                })?
                            };
                            let message = match message_json {
                                JsonValue::String(message) => message,
                                _ => return Err(HandleMethodError::InvalidParams {
                                    message: format!("message parameter must be a string"),
                                    data: None,
                                }),
                            };
                            let delay_json = {
                                values
                                .pop()
                                .ok_or_else(|| HandleMethodError::InvalidParams {
                                    message: format!("missing delay parameter"),
                                    data: None,
                                })?
                            };
                            if !values.is_empty() {
                                return Err(HandleMethodError::InvalidParams {
                                    message: format!("too many parameters"),
                                    data: None,
                                });
                            }
                            let delay = {
                                delay_json
                                .as_u64()
                                .ok_or_else(|| HandleMethodError::InvalidParams {
                                    message: format!("delay must be a positive integer"),
                                    data: None,
                                })?
                            };
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                            Ok(JsonValue::String(message))
                        },
                        JsonRpcParams::Object(_) => {
                            return Err(HandleMethodError::InvalidParams {
                                message: format!("expected array of parameters"),
                                data: None,
                            });
                        },
                    }
                },
                _ => Err(HandleMethodError::MethodNotFound),
            }
        }

        async fn handle_notification<'m>(
            &self,
            _method: &'m str,
            _params: Option<JsonRpcParams>,
        ) {
            unimplemented!()
        }
    }
    */

    #[tokio::test]
    async fn start_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let client_stream = {
            let listener_stream = listener.incoming();
            listener_stream
            .filter_map(|result| async {
                match result {
                    Ok(client) => {
                        Some((client, TestService))
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
            tcp_stream.shutdown().await.unwrap();
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


impl IntoJsonRpcError for JsonRpcError {
    fn into_json_rpc_error(self) -> JsonRpcError {
        self
    }
}

struct TestService;

#[json_rpc_service]
impl TestService {
    #[method = "delay"]
    async fn delay(millis: u32, reply: String) -> Result<String, String> {
        Ok(reply)
    }
}
