use super::*;

pub enum HandleMethodError {
    DropConnection,
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

pub struct DropConnection;

impl From<JsonRpcError> for HandleMethodError {
    fn from(json_rpc_error: JsonRpcError) -> HandleMethodError {
        HandleMethodError::ApplicationError(json_rpc_error)
    }
}

impl From<Infallible> for HandleMethodError {
    fn from(infallible: Infallible) -> HandleMethodError {
        match infallible {}
    }
}

impl From<DropConnection> for HandleMethodError {
    fn from(DropConnection: DropConnection) -> HandleMethodError {
        HandleMethodError::DropConnection
    }
}

impl HandleMethodError {
    pub fn into_json_rpc_error(self, method: &str) -> Option<JsonRpcError> {
        match self {
            HandleMethodError::DropConnection => None,
            HandleMethodError::MethodNotFound => {
                Some(JsonRpcError {
                    code: JsonRpcErrorCode::METHOD_NOT_FOUND,
                    message: format!("method '{}' not found", method),
                    data: None,
                })
            },
            HandleMethodError::InvalidParams { message, data } => {
                Some(JsonRpcError {
                    code: JsonRpcErrorCode::INVALID_PARAMS,
                    message: format!("invalid params: {}", message),
                    data,
                })
            },
            HandleMethodError::InternalError { message, data } => {
                Some(JsonRpcError {
                    code: JsonRpcErrorCode::INTERNAL_ERROR,
                    message,
                    data,
                })
            },
            HandleMethodError::ApplicationError(err) => Some(err),
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

pub struct NullService;

#[async_trait]
impl JsonRpcService for NullService {
    async fn handle_method<'s, 'm>(
        &'s self,
        _method: &'m str,
        _params: Option<JsonRpcParams>,
    ) -> Result<JsonValue, HandleMethodError> {
        Err(HandleMethodError::MethodNotFound)
    }

    async fn handle_notification<'s, 'm>(
        &'s self,
        _method: &'m str,
        _params: Option<JsonRpcParams>,
    ) {
    }
}

