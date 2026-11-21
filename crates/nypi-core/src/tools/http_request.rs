//! `http_request`: one HTTP request, with a secret used on the agent's behalf (decision D77).
//! The agent names a credential by its handle (`cred://<scope>/<name>`) in `auth`; the harness
//! opens it from the secret store, puts it in the request, and scrubs every secret's value out
//! of what comes back before the model, the session, or the audit log sees any of it. The
//! Inspector asks the owner before a credential is used, unless the settings trust that
//! credential's scope for the request's host (`approval.trustedCredentials`).
//!
//! Redirects are not followed: a redirect is reported with its `Location`, so a credential is
//! never carried to a host the call did not name and the owner did not approve.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use nypi_vault::secrets::{Handle, Secret, SecretScrubber, SecretStore};
use serde_json::{Value, json};

use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head};

const DEFAULT_TIMEOUT_SECONDS: f64 = 30.0;
const MAX_TIMEOUT_SECONDS: f64 = 300.0;
/// The most of a response's body that is read at all.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

pub struct HttpRequestTool {
    /// NyPi's home, where the secret store is; opened when a call first names a credential.
    pub home: PathBuf,
    /// A store given directly (tests), instead of the one in `home`.
    pub store: Option<Arc<SecretStore>>,
}

/// How a request names the credential it wants used.
#[derive(Debug, PartialEq)]
pub(crate) enum Auth {
    Bearer { cred: Handle },
    Header { name: String, cred: Handle },
    Basic { username: String, password_cred: Handle },
}

impl Auth {
    /// The `auth` argument, when there is one; an error the model can act on when it is not
    /// one of the three forms.
    pub(crate) fn read(args: &Value) -> Result<Option<Auth>, String> {
        let auth = &args["auth"];
        if auth.is_null() {
            return Ok(None);
        }
        let cred = |key: &str| Handle::parse(auth[key].as_str().unwrap_or_default());
        match auth["type"].as_str().map(str::to_lowercase).as_deref() {
            Some("bearer") => Ok(Some(Auth::Bearer { cred: cred("cred")? })),
            Some("header") => match auth["name"].as_str().map(str::trim).filter(|name| !name.is_empty()) {
                Some(name) => Ok(Some(Auth::Header { name: name.to_string(), cred: cred("cred")? })),
                None => Err("auth of type header needs `name`, the header to set".into()),
            },
            Some("basic") => Ok(Some(Auth::Basic {
                username: auth["username"].as_str().unwrap_or_default().to_string(),
                password_cred: cred("passwordCred")?,
            })),
            _ => Err("auth.type must be bearer, header, or basic".into()),
        }
    }

    pub(crate) fn handle(&self) -> &Handle {
        match self {
            Auth::Bearer { cred } | Auth::Header { cred, .. } => cred,
            Auth::Basic { password_cred, .. } => password_cred,
        }
    }
}

/// The host a URL names, lowercased, or None when it is not an http(s) URL.
pub(crate) fn host_of(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("https://").or_else(|| url.trim().strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    // An IPv6 literal keeps its brackets; anything else loses its port.
    let host = if host.starts_with('[') {
        host.split(']').next().map(|h| format!("{h}]"))?
    } else {
        host.split(':').next()?.to_string()
    };
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// Whether the settings trust the credentials of `scope` for `host`: the host itself, or
/// `*.example.com` for it and every host below it. `*` alone trusts the scope everywhere.
pub(crate) fn scope_trusted(trusted: &BTreeMap<String, Vec<String>>, scope: &str, host: &str) -> bool {
    trusted.get(scope).is_some_and(|hosts| {
        hosts.iter().map(|pattern| pattern.trim().to_lowercase()).any(|pattern| match pattern.strip_prefix("*.") {
            Some(domain) => host == domain || host.ends_with(&format!(".{domain}")),
            None => pattern == "*" || pattern == host,
        })
    })
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "http_request".into(),
            description: "Make one HTTP request and read the response (status, headers, body). To authenticate, \
                name a credential by its handle in `auth`; you never see or handle its value: the harness puts it in \
                the request and removes it from everything that comes back. The user is asked before a credential \
                is used, unless they trusted it for that host. Redirects are not followed: a 3xx is returned with \
                its Location for you to decide on. Use it for APIs; use bash for anything else."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"], "description": "Default GET" },
                    "url": { "type": "string", "description": "http:// or https://" },
                    "headers": { "type": "object", "description": "Extra request headers (never a secret: use auth)" },
                    "body": { "type": "string", "description": "The request body as text" },
                    "json": { "description": "The request body as JSON (sets Content-Type)" },
                    "auth": {
                        "type": "object",
                        "description": "One of: {type: bearer, cred}, {type: header, name, cred}, {type: basic, username, passwordCred}; cred is a handle such as cred://github/lucia-bot",
                        "properties": {
                            "type": { "type": "string", "enum": ["bearer", "header", "basic"] },
                            "cred": { "type": "string" },
                            "name": { "type": "string" },
                            "username": { "type": "string" },
                            "passwordCred": { "type": "string" }
                        }
                    },
                    "timeout": { "type": "number", "description": "Seconds (default 30)" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let url = args["url"].as_str().unwrap_or_default().trim().to_string();
        if host_of(&url).is_none() {
            return Ok(ToolOutput::error("`url` must be an http:// or https:// URL"));
        }
        let method = args["method"].as_str().unwrap_or("GET").to_uppercase();
        let Ok(method) = reqwest::Method::from_bytes(method.as_bytes()) else {
            return Ok(ToolOutput::error(format!("Unknown method: {method}")));
        };
        let auth = match Auth::read(&args) {
            Ok(auth) => auth,
            Err(error) => return Ok(ToolOutput::error(error)),
        };
        // A header the model writes must not carry a secret in the clear: that is what `auth` is for.
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(given) = args["headers"].as_object() {
            for (name, value) in given {
                let value = value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string());
                if let Some(what) = nypi_vault::secrets::looks_secret(&value) {
                    return Ok(ToolOutput::error(format!(
                        "The header {name} looks like it holds {what}. Secrets are never written into a call: name \
                         the credential in `auth` by its cred:// handle."
                    )));
                }
                headers.push((name.clone(), value));
            }
        }

        // Every secret is looked for in what comes back, not only the one this call uses: a
        // server may echo another.
        let mut scrubber = SecretScrubber::default();
        let mut secret: Option<Secret> = None;
        if let Some(auth) = &auth {
            let store = match &self.store {
                Some(store) => store.clone(),
                None => match SecretStore::open(&self.home) {
                    Ok(store) => Arc::new(store),
                    Err(error) => {
                        return Ok(ToolOutput::error(format!("The secret store could not be opened: {error:#}")));
                    }
                },
            };
            scrubber = SecretScrubber::new(store.all());
            match store.get(auth.handle()) {
                Ok(Some(found)) => secret = Some(found),
                Ok(None) => {
                    return Ok(ToolOutput::error(format!(
                        "There is no secret {}. The user stores one with: nypi secrets set {}/{}",
                        auth.handle(),
                        auth.handle().scope,
                        auth.handle().name
                    )));
                }
                Err(error) => return Ok(ToolOutput::error(format!("{error:#}"))),
            }
        }

        let timeout = args["timeout"]
            .as_f64()
            .filter(|t| *t > 0.0 && *t <= MAX_TIMEOUT_SECONDS)
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs_f64(timeout))
            .user_agent(concat!("nypi/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let mut request = client.request(method.clone(), &url);
        for (name, value) in &headers {
            request = request.header(name, value);
        }
        match (&auth, &secret) {
            (Some(Auth::Bearer { .. }), Some(secret)) => request = request.bearer_auth(secret.expose()),
            (Some(Auth::Header { name, .. }), Some(secret)) => request = request.header(name, secret.expose()),
            (Some(Auth::Basic { username, password_cred }), Some(secret)) => {
                // The form the header carries gives the password away as surely as the password.
                let pair = format!("{username}:{}", secret.expose());
                scrubber.also(password_cred, &pair);
                scrubber.also(password_cred, &base64::engine::general_purpose::STANDARD.encode(&pair));
                request = request.basic_auth(username, Some(secret.expose()));
            }
            _ => {}
        }
        if !args["json"].is_null() {
            request = request.json(&args["json"]);
        } else if let Some(body) = args["body"].as_str() {
            request = request.body(body.to_string());
        }

        let sent = tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(ToolOutput::error("Operation aborted")),
            sent = request.send() => sent,
        };
        let response = match sent {
            // An error's text can quote the URL and, from some stacks, a header: scrubbed too.
            Err(error) => return Ok(ToolOutput::error(scrubber.scrub(&format!("The request failed: {error}")))),
            Ok(response) => response,
        };
        let status = response.status();
        let mut head =
            format!("HTTP {} {}", status.as_u16(), status.canonical_reason().unwrap_or_default()).trim().to_string();
        for (name, value) in response.headers() {
            head.push_str(&format!("\n{name}: {}", String::from_utf8_lossy(value.as_bytes())));
        }
        let bytes = tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(ToolOutput::error("Operation aborted")),
            bytes = response.bytes() => bytes.unwrap_or_default(),
        };
        let total = bytes.len();
        let kept = &bytes[..total.min(MAX_BODY_BYTES)];
        let body = if kept.iter().take(8192).any(|byte| *byte == 0) {
            format!("(binary body, {total} bytes)")
        } else {
            let truncation = truncate_head(&String::from_utf8_lossy(kept), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
            let mut body = truncation.text;
            if truncation.truncated || total > kept.len() {
                body.push_str(&format!("\n\n[Body truncated; {total} bytes in all]"));
            }
            body
        };
        let text = scrubber.scrub(&format!("{head}\n\n{body}"));
        let mut output = ToolOutput::text(text).with_details(json!({
            "method": method.as_str(),
            "url": scrubber.scrub(&url),
            "status": status.as_u16(),
            "bytes": total,
            "cred": auth.as_ref().map(|auth| auth.handle().to_string()),
        }));
        output.is_error = status.is_client_error() || status.is_server_error();
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_trusted_scopes_are_read_strictly() {
        assert_eq!(host_of("https://API.GitHub.com/user?x=1").as_deref(), Some("api.github.com"));
        assert_eq!(host_of("http://user:pw@localhost:8080/a").as_deref(), Some("localhost"));
        assert_eq!(host_of("http://[::1]:9000/").as_deref(), Some("[::1]"));
        assert_eq!(host_of("file:///etc/passwd"), None);
        assert_eq!(host_of("https:///nothing"), None);

        let trusted = BTreeMap::from([
            ("github".to_string(), vec!["api.github.com".to_string()]),
            ("corp".to_string(), vec!["*.example.com".to_string()]),
        ]);
        assert!(scope_trusted(&trusted, "github", "api.github.com"));
        assert!(
            !scope_trusted(&trusted, "github", "evil.com")
                && !scope_trusted(&trusted, "github", "api.github.com.evil.com")
        );
        assert!(scope_trusted(&trusted, "corp", "example.com") && scope_trusted(&trusted, "corp", "a.b.example.com"));
        assert!(!scope_trusted(&trusted, "corp", "notexample.com") && !scope_trusted(&trusted, "other", "example.com"));
    }

    #[test]
    fn auth_names_a_credential_by_handle_in_one_of_three_forms() {
        let bearer =
            Auth::read(&json!({ "auth": { "type": "bearer", "cred": "cred://github/bot" } })).unwrap().unwrap();
        assert_eq!(bearer.handle().to_string(), "cred://github/bot");
        let basic = json!({ "auth": { "type": "Basic", "username": "lucia", "passwordCred": "mail/smtp" } });
        assert!(matches!(Auth::read(&basic), Ok(Some(Auth::Basic { username, .. })) if username == "lucia"));
        assert_eq!(Auth::read(&json!({})), Ok(None));
        for bad in [
            json!({ "auth": { "type": "bearer", "cred": "sk-raw-secret-value" } }),
            json!({ "auth": { "type": "header", "cred": "a/b" } }),
            json!({ "auth": { "type": "digest", "cred": "a/b" } }),
        ] {
            assert!(Auth::read(&bad).is_err(), "{bad}");
        }
    }
}
