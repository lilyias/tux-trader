use thiserror::Error;

#[derive(Debug, Error)]
pub enum TuxError {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("invalid instrument: {0}")]
    InvalidInstrument(String),
    #[error("invalid order: {0}")]
    InvalidOrder(String),
    #[error("illegal order state transition: {from} -> {to}")]
    IllegalOrderTransition { from: String, to: String },
    #[error("risk rejected [{rule}]: {message}")]
    RiskRejected { rule: String, message: String },
    #[error("stale or missing market data for {instrument}")]
    StaleMarketData { instrument: String },
    #[error("{venue} exchange error: {message}")]
    Exchange { venue: String, message: String },
    #[error("storage error: {0}")]
    Storage(String),
    #[error("{0}")]
    Other(String),
}

impl TuxError {
    pub fn other(msg: impl Into<String>) -> Self {
        TuxError::Other(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, TuxError>;
