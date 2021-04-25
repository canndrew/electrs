use super::*;

pub struct ResponseMap {
    next_id: i64,
    response_map: HashMap<i64, oneshot::Sender<Result<JsonValue, JsonRpcError>>>,
    batch_response_map: HashMap<i64, oneshot::Sender<Vec<Result<JsonValue, JsonRpcError>>>>,
}

impl ResponseMap {
    pub fn insert_single(&mut self)
        -> (i64, oneshot::Receiver<Result<JsonValue, JsonRpcError>>)
    {
        let (result_sender, result_receiver) = oneshot::channel();
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.response_map.insert(id, result_sender);
        (id, result_receiver)
    }

    pub fn insert_batch(&mut self, len: usize)
        -> (i64, oneshot::Receiver<Vec<Result<JsonValue, JsonRpcError>>>)
    {
        let (result_sender, result_receiver) = oneshot::channel();
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(len as i64);
        self.batch_response_map.insert(id, result_sender);
        (id, result_receiver)
    }
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

pub struct JsonRpcSession<A> {
    message_reader: JsonRpcMessageReader<tokio::io::BufReader<tokio::io::ReadHalf<A>>>,
    message_writer: Arc<Mutex<JsonRpcMessageWriter<tokio::io::WriteHalf<A>>>>,
    response_map: Arc<Mutex<ResponseMap>>,
}

impl<A> JsonRpcSession<A>
where
    A: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(connection: A) -> (JsonRpcSession<A>, JsonRpcClient<tokio::io::WriteHalf<A>>) {
        let (reader, writer) = tokio::io::split(connection);
        let reader = BufReader::new(reader);
        let message_reader = JsonRpcMessageReader::new(reader);
        let message_writer = Arc::new(Mutex::new(JsonRpcMessageWriter::new(writer)));
        let response_map = Arc::new(Mutex::new(ResponseMap {
            next_id: 0,
            response_map: HashMap::new(),
            batch_response_map: HashMap::new(),
        }));

        let json_rpc_client = JsonRpcClient::from_parts(
            Arc::downgrade(&message_writer),
            Arc::downgrade(&response_map),
        );
        let json_rpc_session = JsonRpcSession {
            message_reader,
            message_writer,
            response_map,
        };
        (json_rpc_session, json_rpc_client)
    }

    pub async fn run(self) -> Result<(), ConnectionError> {
        self.serve(NullService, 1).await
    }

    pub async fn serve<S>(
        self,
        service: S,
        max_concurrent_requests: usize,
    ) -> Result<(), ConnectionError>
    where
        S: JsonRpcService,
    {
        let JsonRpcSession { message_reader, message_writer, response_map } = self;
        let mut active_requests = FuturesUnordered::new();
        let message_stream = message_reader.into_stream().fuse();
        pin_mut!(message_stream);
        loop {
            let response_opt = if active_requests.len() < max_concurrent_requests {
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
                                            let response = handle_method_call(
                                                service, id, &method, params,
                                            ).await;
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
                                                let response = handle_method_call(
                                                    service, id, &method, params,
                                                ).await;
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
    }
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

async fn handle_method_call<S>(
    service: &S,
    id: JsonRpcId,
    method: &str,
    params: Option<JsonRpcParams>,
) -> JsonRpcResponse
where
    S: JsonRpcService,
{
    let result = service.handle_method(&method, params).await;
    JsonRpcResponse {
        id,
        result: result.map_err(|err| err.into_json_rpc_error(&method)),
    }
}

