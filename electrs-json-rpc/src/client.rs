use super::*;

pub struct JsonRpcClient<A> {
    message_writer: Weak<Mutex<JsonRpcMessageWriter<A>>>,
    response_map: Weak<Mutex<ResponseMap>>,
}

impl<A> JsonRpcClient<A>
where
    A: AsyncWrite + Unpin,
{
    pub(crate) fn from_parts(
        message_writer: Weak<Mutex<JsonRpcMessageWriter<A>>>,
        response_map: Weak<Mutex<ResponseMap>>,
    ) -> JsonRpcClient<A> {
        JsonRpcClient { message_writer, response_map }
    }

    pub async fn notify(
        &self,
        method: &str,
        params: Option<JsonRpcParams>,
    ) -> Result<(), ClientSendRequestError> {
        let request = JsonRpcRequest {
            id: None,
            method: method.to_owned(),
            params,
        };
        let message = JsonRpcMessage::Request(JsonRpcRequests::Single(request));
        let message_writer = match self.message_writer.upgrade() {
            Some(message_writer) => message_writer,
            None => return Err(ClientSendRequestError::ConnectionDropped),
        };
        let mut message_writer = message_writer.lock().await;
        match message_writer.write_message(message).await {
            Ok(()) => Ok(()),
            Err(source) => Err(ClientSendRequestError::Io { source }),
        }
    }

    pub async fn call_method(
        &self,
        method: &str,
        params: Option<JsonRpcParams>,
    ) -> Result<Result<JsonValue, JsonRpcError>, ClientSendRequestError> {
        let (id, result_receiver) = {
            let response_map = match self.response_map.upgrade() {
                Some(response_map) => response_map,
                None => return Err(ClientSendRequestError::ConnectionDropped),
            };
            let mut response_map = response_map.lock().await;
            response_map.insert_single()
        };
        let request = JsonRpcRequest {
            id: Some(JsonRpcId::Number(id)),
            method: method.to_owned(),
            params,
        };
        let message = JsonRpcMessage::Request(JsonRpcRequests::Single(request));
        let result = {
            let message_writer = match self.message_writer.upgrade() {
                Some(message_writer) => message_writer,
                None => return Err(ClientSendRequestError::ConnectionDropped),
            };
            let mut message_writer = message_writer.lock().await;
            message_writer.write_message(message).await
        };
        match result {
            Ok(()) => (),
            Err(source) => return Err(ClientSendRequestError::Io { source }),
        }
        match result_receiver.await {
            Ok(result) => Ok(result),
            Err(_recv_error) => Err(ClientSendRequestError::ConnectionDropped),
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

    pub async fn send(self) -> Result<Vec<Result<JsonValue, JsonRpcError>>, ClientSendRequestError> {
        let num_method_calls = self.method_calls.len();
        let (mut id, result_receiver) = {
            let response_map = match self.json_rpc_client.response_map.upgrade() {
                Some(response_map) => response_map,
                None => return Err(ClientSendRequestError::ConnectionDropped),
            };
            let mut response_map = response_map.lock().await;
            response_map.insert_batch(num_method_calls)
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
                None => return Err(ClientSendRequestError::ConnectionDropped),
            };
            let mut message_writer = message_writer.lock().await;
            message_writer.write_message(message).await
        };
        match result {
            Ok(()) => (),
            Err(source) => return Err(ClientSendRequestError::Io { source }),
        }
        match result_receiver.await {
            Ok(result) => Ok(result),
            Err(_recv_error) => Err(ClientSendRequestError::ConnectionDropped),
        }
    }
}

#[derive(Debug, Error)]
pub enum ClientSendRequestError {
    #[error("io error sending request: {}", source)]
    Io {
        source: io::Error,
    },
    #[error("the connection to the server has been dropped")]
    ConnectionDropped,
}

#[derive(Debug, Error)]
pub enum ClientSendNotificationError {
    #[error("io error sending notification: {}", source)]
    Io {
        source: io::Error,
    },
    #[error("connection with server has been dropped")]
    ConnectionDropped,
    #[error("error serializing parameters: {}", source)]
    Serialize {
        source: serde_json::Error,
    },
}

#[derive(Debug, Error)]
pub enum ClientCallMethodError<E>
where
    E: fmt::Debug + std::error::Error + 'static,
{
    #[error("io error calling method: {}", source)]
    Io {
        source: io::Error,
    },
    #[error("connection with server has been dropped")]
    ConnectionDropped,
    #[error("error serializing parameters: {}", source)]
    Serialize {
        source: serde_json::Error,
    },
    #[error("error parsing response: {}", source)]
    ParseResponse {
        source: E,
    },
}

