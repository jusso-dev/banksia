use thiserror::Error;

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("Graph returned {status} {code}: {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },
    #[error("Graph request failed: {0}")]
    Transport(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("refused: {0}")]
    Refused(String),
    #[error("unexpected Graph response: {0}")]
    Decode(String),
}

impl GraphError {
    pub fn status(&self) -> Option<u16> {
        match self {
            GraphError::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Delta state is no longer valid and a full resync is required.
    pub fn is_sync_state_expired(&self) -> bool {
        match self {
            GraphError::Api { status, code, .. } => {
                *status == 410
                    || matches!(
                        code.as_str(),
                        "syncStateNotFound"
                            | "resyncRequired"
                            | "syncStateInvalid"
                            | "resyncApplyDifferences"
                    )
            }
            _ => false,
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }

    pub fn is_conflict(&self) -> bool {
        self.status() == Some(409)
    }
}
