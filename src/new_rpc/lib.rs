use std::io;
use tokio::io::{AsyncRead, AsyncWrite, AsyncBufReadExt, BufReader, AsyncWriteExt};
use serde_json::{json, Value as JsonValue};
use async_trait::async_trait;
use futures::{stream, Stream, StreamExt, TryStreamExt};
use tokio_stream::wrappers::SplitStream;

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
        let lines = SplitStream::new((&mut reader).split(b'\n'));
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

#[cfg(test)]
mod test {
    use super::*;

    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use futures::StreamExt;

    struct TestService;

    #[async_trait]
    impl JsonRpcService for TestService {
        async fn handle_method<'m>(
            &self, 
            _method: &'m str,
            _params: Option<JsonRpcParams>,
        ) -> Result<JsonValue, HandleMethodError> {
            unimplemented!()
        }

        async fn handle_notification<'m>(
            &self,
            _method: &'m str,
            _params: Option<JsonRpcParams>,
        ) {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn start_server() {
        let client_stream = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let listener_stream = TcpListenerStream::new(listener);
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
        let _server = run(client_stream, 1, 1, 1);
    }
}









/*
fn main() {
    let connections = {
        let listener = TcpListener::bind("wow")?;
        let listener_stream = TcpListenerStream::new(listener);
        listener_stream
        .filter_map(|result| match result {
            Ok(connection) => {
                let (read_half, write_half) = connection.into_split();
                Some((read_half, write_half, RpcClient))
            },
            Err(err) => log!(error),
        })
    };

    let server = json_rpc::run(connections);

    // We have a Stream of (AsyncRead, AsyncWrite)

    // we then take that stream, and turn it into a stream of RpcClient.
}

struct RpcClient;

#[json_rpc_server]
impl RpcClient {
    #[method = "blockchain.scripthash.subscribe"]
    fn script_hash_subscribe(&self, script_hash: &ScriptHash)
        -> Result<Something, SomeError>
    {
        // do stuff
    }
}

// auto-generated
type SomeFuture = impl Future<Output = ()>;
impl JsonRpcClient for RpcClient {
    type HandleRequestFuture = SomeFuture;

    fn handle_request<W>(&self, request: Request, writer: &mut W) -> SomeFuture {
        match request.method {
            "blockchain.scripthash.subscribe" => {
                if request.params.length() != 1 {
                    return send_invalid_params(writer);
                }
                let script_hash: &ScriptHash = request.params[0].parse();
                let result = self.script_hash_subscribe(script_hash);
            },
            _ => send_invalid_method(writer),
        }
    }
}

struct Complete<S>
where
    S: Stream<Item = ()>,
{
    stream: S,
}

impl<S> Future for Complete<S>
where
    S: Stream<Item = ()>,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        loop {
            match self.stream.poll_next(context) {
                Poll::Ready(Some(())) => (),
                Poll::Ready(None) => return Poll::Ready,
                Poll::NotReady => return Poll::NotReady,
            }
        }
    }
}

#[ext_trait]
impl<S: Stream> StreamExt for Stream {
    fn complete(self) -> Complete<S> {
        Complete { self }
    }
}

mod json_rpc {
    fn run<S, R, W, C>(connections: S)
        -> impl Stream<io::Error>,
    where
        S: Stream<Item = (R, W, C)>,
        R: AsyncRead,
        W: AsyncWrite,
        C: JsonRpcConnection.
    {
        connections
        .map(|connection| tokio::spawn(move || handle_connection(connection)))
        .buffer_unordered()
        .filter_map(|result| match result {
            Ok(()) => None,
            Err(io_error) => Some(io_error),
        })
    }

    async fn handle_connection<R, W, C>(
        reader: R,
        writer: W,
        connection: C,
    ) {
        reader
        .split(b'\n')
        .filter_map(|line_bytes| {
            let line = match String::try_from_utf8(line_bytes) {
                Ok(line) => line,
                Err(utf8_error) => {
                    return send_parse_error(..);
                },
            };
            match parse_request(line) {
                Single(request) => handle_request(request),
                Batch(requests) => {
                    let mut sub_futures = FuturesUnordered::with_capacity(requests.len());
                    for request in requests {
                        sub_futures.push(handle_request(request));
                    }
                    sub_futures
                    .filter_map(|x| x)
                    .collect()
                    .map(|responses| {
                        // spec says not to return an empty array
                        if responses.is_empty() {
                            None
                        } else {
                            Some(responses)
                        }
                    })
                },
            }
        })
        .try_buffer_unordered() // is this okay?
        .try_filter_map(|x| x)
        .try_then(|response| async {
            writer.write_all(response.into_json()).await;
            writer.write('\n').await;
        })
        .try_collect()
    }

    async fn handle_request(request: Request) -> Option<Respose> {
        let Request { id, method, params } = request;
        match id {
            Some(id) => {
                let response = connection.handle_method(method, params);
                Some(response)
            },
            None => {
                connection.handle_notification(method, params);
                None
            },
        }
    }
}
*/
