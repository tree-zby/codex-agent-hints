//! Manage requests (protocol sections 9.2 and 9.3).
//!
//! A manage request is a *separate* HTTP call with a placeholder Responses
//! body. The transport is left to the caller so this crate stays
//! dependency-free; the `reqwest` wiring lives in the integration snippet.

use serde_json::{json, Value};

use crate::encoder::encode;
use crate::protocol::{validate, State, HEADER_NAME};
use crate::state::{HintError, HintSession};

/// A fully materialised manage request, ready to hand to any HTTP client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManageRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Builds manage requests for one Codex provider.
#[derive(Debug, Clone)]
pub struct ManageClient {
    base_url: String,
    model: String,
    bearer: Option<String>,
}

impl ManageClient {
    #[must_use]
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        let base_url = base_url.into();
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.into(),
            bearer: None,
        }
    }

    /// Reuse the host's `Authorization: Bearer` credential.
    #[must_use]
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer = Some(token.into());
        self
    }

    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("{}/responses", self.base_url)
    }

    /// Materialise a manage request from an already built hint.
    pub fn build(&self, hint: &Value) -> Result<ManageRequest, HintError> {
        let errors = validate(hint);
        if !errors.is_empty() {
            return Err(HintError::InvalidConfig(errors.join("; ")));
        }
        let value = encode(hint).map_err(|error| HintError::InvalidConfig(error.to_string()))?;

        let mut headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            (HEADER_NAME.to_string(), value),
        ];
        if let Some(bearer) = &self.bearer {
            headers.push(("authorization".to_string(), format!("Bearer {bearer}")));
        }

        // The body is a fixed placeholder: a manage request carries no prompt.
        // Codex's wire API is the Responses API, so `input` is empty.
        let body = json!({
            "model": self.model,
            "input": [],
            "stream": false,
        })
        .to_string();

        Ok(ManageRequest {
            method: "POST",
            url: self.endpoint(),
            headers,
            body,
        })
    }

    /// Build the `compact` manage request and advance the local epoch.
    ///
    /// The rewrite is recorded here because the hint itself carries the
    /// *superseded* epoch: the next ordinary request must advertise epoch + 1
    /// and must not carry `cache_control` yet.
    pub fn compact(&self, session: &mut HintSession) -> Result<ManageRequest, HintError> {
        let hint = session.build_manage(State::Compact, None, None)?;
        let request = self.build(&hint)?;
        session.note_rewrite();
        Ok(request)
    }

    /// Build a lifecycle manage request without mutating sender state.
    pub fn lifecycle(
        &self,
        session: &mut HintSession,
        verb: State,
    ) -> Result<ManageRequest, HintError> {
        let hint = session.build_manage(verb, None, None)?;
        self.build(&hint)
    }
}

/// Lifecycle verbs accepted from an operator surface.
#[must_use]
pub fn parse_verb(value: &str) -> Option<State> {
    match value {
        "pause" => Some(State::Pause),
        "resume" => Some(State::Resume),
        "stop" => Some(State::Stop),
        "compact" => Some(State::Compact),
        _ => None,
    }
}
