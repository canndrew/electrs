use super::*;

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
        possible_batch_request: bool,
    },
    #[error("error parsing json-rpc message: {}", source)]
    Parse {
        source: JsonRpcMessageFromJsonError,
    },
}

impl ReadMessageError {
    pub fn as_error_response(&self) -> Option<JsonRpcResponse> {
        match self {
            ReadMessageError::Io { .. } => None,
            ReadMessageError::Deserialize { source, possible_batch_request } => {
                if *possible_batch_request {
                    Some(JsonRpcResponse {
                        id: JsonRpcId::Null,
                        result: Err(JsonRpcError {
                            code: JsonRpcErrorCode::INVALID_REQUEST,
                            message: source.to_string(),
                            data: None,
                        }),
                    })
                } else {
                    None
                }
            },
            ReadMessageError::Parse { source } => source.as_error_response(),
        }
    }
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
            Err(source) => {
                let possible_batch_request = {
                    let trimmed = line.trim();
                    trimmed.starts_with("[") && trimmed.ends_with("]")
                };
                return Err(ReadMessageError::Deserialize { source, possible_batch_request });
            },
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

