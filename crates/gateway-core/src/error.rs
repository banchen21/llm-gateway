//! Defines the error type shared by the whole gateway.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The gateway itself is misconfigured. Not the caller's fault, so it must not be
    /// reported as a client error.
    #[error("configuration error: {0}")]
    Config(String),

    /// The caller's request cannot be served, carrying the status to report.
    #[error("{message}")]
    Request { status: u16, message: String },

    /// An upstream answered with a non-success status.
    ///
    /// The status is carried through rather than collapsed into a generic gateway
    /// failure, so a caller still sees an upstream 401 or 429 and can react to it.
    #[error("upstream error {status}: {message}")]
    Upstream { status: u16, message: String },

    /// An upstream failed, or violated its protocol, after the response was committed.
    #[error("provider error: {0}")]
    Provider(String),

    /// A transport failure reaching or reading from an upstream.
    ///
    /// The whole source chain is rendered, because `reqwest`'s top-level message is only
    /// ever "error sending request" — the actual cause (DNS, TLS, connection reset) is a
    /// nested source and is the only part worth showing an operator.
    #[error("{}", transport_message(.0))]
    Http(#[from] reqwest::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

fn transport_message(error: &reqwest::Error) -> String {
    let mut message = format!("upstream transport error: {error}");
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(" -> ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

impl Error {
    /// Whether another provider might answer where this one refused.
    ///
    /// These are the refusals that are about the account rather than the request: quota
    /// exhausted (429), a rejected credential (401), or a plan that excludes the model
    /// (403). A malformed request would fail everywhere, so it is not included.
    pub fn is_provider_refusal(&self) -> bool {
        matches!(self, Error::Upstream { status, .. } if matches!(status, 401 | 403 | 429))
    }

    /// The HTTP status this error should surface as at the gateway's edge.
    pub fn http_status(&self) -> u16 {
        match self {
            Error::Config(_) => 500,
            Error::Request { status, .. } => *status,
            Error::Upstream { status, .. } if (400..600).contains(status) => *status,
            Error::Upstream { .. } => 502,
            Error::Provider(_) => 502,
            Error::Http(error) if error.is_timeout() => 504,
            Error::Http(_) | Error::Io(_) => 502,
            Error::Json(_) => 400,
        }
    }

    /// The error type string Anthropic clients expect in an error envelope.
    pub fn anthropic_error_type(&self) -> &'static str {
        match self.http_status() {
            400 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            404 => "not_found_error",
            413 => "request_too_large",
            429 => "rate_limit_error",
            504 => "timeout_error",
            529 => "overloaded_error",
            _ => "api_error",
        }
    }

    /// The error type string OpenAI clients expect in an error envelope.
    pub fn openai_error_type(&self) -> &'static str {
        match self.http_status() {
            400 | 404 | 413 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            429 => "rate_limit_error",
            _ => "server_error",
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Error::Config(message.into())
    }

    /// The caller sent something this gateway cannot serve.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Error::Request {
            status: 400,
            message: message.into(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Error::Request {
            status: 401,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Error::Request {
            status: 404,
            message: message.into(),
        }
    }

    pub fn provider(message: impl Into<String>) -> Self {
        Error::Provider(message.into())
    }

    pub fn upstream(status: reqwest::StatusCode, message: impl Into<String>) -> Self {
        Error::Upstream {
            status: status.as_u16(),
            message: message.into(),
        }
    }
}
