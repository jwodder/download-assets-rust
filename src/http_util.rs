use indent_write::indentable::Indentable;
use mime::{Mime, JSON};
use reqwest::Response;
use serde_json::{to_string_pretty, value::Value};

/// Error raised for a 4xx or 5xx HTTP response that includes the response body
#[derive(Debug)]
pub struct StatusError {
    url: reqwest::Url,
    status: reqwest::StatusCode,
    body: Option<String>,
}

impl StatusError {
    /// If the given response has a 4xx or 5xx status code, construct & return
    /// a `StatusError`; otherwise, return the response unchanged.
    pub async fn error_for_status(r: Response) -> Result<Response, StatusError> {
        let status = r.status();
        if status.is_client_error() || status.is_server_error() {
            let url = r.url().clone();
            // If the response body is JSON, pretty-print it.
            let body = if is_json_response(&r) {
                r.json::<Value>()
                    .await
                    .ok()
                    .map(|v| to_string_pretty(&v).unwrap())
            } else {
                r.text().await.ok()
            };
            Err(StatusError { url, status, body })
        } else {
            Ok(r)
        }
    }
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Request to {} returned {}", self.url, self.status)?;
        if let Some(text) = &self.body {
            write!(f, "\n\n{}\n", text.indented("    "))?;
        }
        Ok(())
    }
}

impl std::error::Error for StatusError {}

/// Return the `rel="next"` URL, if any, from the response's "Link" header
pub fn get_next_link(r: &Response) -> Option<String> {
    let header_value = r.headers().get(reqwest::header::LINK)?.to_str().ok()?;
    Some(
        parse_link_header::parse_with_rel(header_value)
            .ok()?
            .get("next")?
            .raw_uri
            .clone(),
    )
}

/// Returns `true` iff the response's Content-Type header indicates the body is
/// JSON
pub fn is_json_response(r: &Response) -> bool {
    r.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<Mime>().ok())
        .map(|ct| {
            ct.type_() == "application" && (ct.subtype() == "json" || ct.suffix() == Some(JSON))
        })
        .unwrap_or(false)
}
