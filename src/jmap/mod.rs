pub mod mail;

use crate::config::AccountConfig;
use crate::error::AppError;
use jmap_client::client::Client;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub struct JmapClient {
    client: Client,
    account_id: String,
    username: String,
}

impl JmapClient {
    pub async fn connect(config: &AccountConfig) -> Result<Self, AppError> {
        let password = config.password()?;

        let url_parsed = url::Url::parse(&config.url)
            .map_err(|e| AppError::JmapConnection(format!("Invalid URL: {e}")))?;
        let host = url_parsed
            .host_str()
            .ok_or_else(|| AppError::JmapConnection("No host in URL".into()))?
            .to_string();

        let client = Client::new()
            .credentials((config.username.as_str(), password.as_str()))
            .follow_redirects([host])
            .connect(&config.url)
            .await
            .map_err(|e| AppError::JmapConnection(e.to_string()))?;

        let account_id = client.default_account_id().to_string();
        let username = client.session().username().to_string();

        tracing::info!(
            name = %config.name,
            url = %config.url,
            account_id = %account_id,
            "Connected to Stalwart via JMAP"
        );

        Ok(Self {
            client,
            account_id,
            username,
        })
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// Start an EventSource stream for real-time change notifications.
    /// Uses our own reqwest client with HTTP/1.1 forced, because jmap-client's
    /// built-in event_source() may use HTTP/2 which doesn't stream SSE correctly
    /// with some servers.
    pub async fn event_source(
        &self,
        ping_interval: u32,
    ) -> Result<
        impl futures::Stream<Item = jmap_client::Result<jmap_client::event_source::PushNotification>>
        + Unpin,
        AppError,
    > {
        use jmap_client::core::session::URLPart;
        use jmap_client::event_source::parser::EventParser;
        use reqwest::header::{ACCEPT, HeaderValue};

        // Build the EventSource URL from the JMAP session
        let mut url = String::new();
        for part in self.client.event_source_url() {
            match part {
                URLPart::Value(value) => url.push_str(value),
                URLPart::Parameter(param) => match param {
                    jmap_client::event_source::URLParameter::Types => {
                        url.push_str("Email,Mailbox,EmailDelivery");
                    }
                    jmap_client::event_source::URLParameter::CloseAfter => {
                        url.push_str("no");
                    }
                    jmap_client::event_source::URLParameter::Ping => {
                        url.push_str(&ping_interval.to_string());
                    }
                },
            }
        }

        // Build HTTP/1.1-only client — critical for SSE streaming
        let headers = self.client.headers().clone();
        let http_client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .http1_only()
            .default_headers(headers)
            .build()
            .map_err(|e| AppError::JmapConnection(format!("Failed to build HTTP client: {e}")))?;

        let response = http_client
            .get(&url)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
            .send()
            .await
            .map_err(|e| AppError::JmapConnection(format!("EventSource request failed: {e}")))?;

        if !response.status().is_success() {
            return Err(AppError::JmapConnection(format!(
                "EventSource returned HTTP {}",
                response.status()
            )));
        }

        tracing::debug!(url = %url, "EventSource SSE connection established");

        let mut stream = response.bytes_stream();
        let mut parser = EventParser::default();

        Ok(Box::pin(async_stream::stream! {
            loop {
                if let Some(notification) = parser.filter_notification() {
                    yield notification;
                    continue;
                }
                if let Some(result) = futures::StreamExt::next(&mut stream).await {
                    match result {
                        Ok(bytes) => {
                            parser.push_bytes(bytes.to_vec());
                            continue;
                        }
                        Err(err) => {
                            yield Err(err.into());
                            break;
                        }
                    }
                } else {
                    break;
                }
            }
        }))
    }
}

pub struct AccountManager {
    accounts: HashMap<String, Arc<JmapClient>>,
    default: String,
}

impl AccountManager {
    pub async fn connect_all(
        configs: &[AccountConfig],
        default_account: Option<&str>,
    ) -> Result<Self, AppError> {
        if configs.is_empty() {
            return Err(AppError::Config("No accounts configured".into()));
        }

        let mut names = HashSet::new();
        for config in configs {
            if config.name.trim().is_empty() || !names.insert(config.name.as_str()) {
                return Err(AppError::Config(
                    "Account names must be nonempty and unique".into(),
                ));
            }
        }
        if let Some(name) = default_account
            && !names.contains(name)
        {
            return Err(AppError::Config(format!(
                "Default account '{name}' is not configured"
            )));
        }

        let mut accounts = HashMap::new();
        for config in configs {
            match JmapClient::connect(config).await {
                Ok(client) => {
                    accounts.insert(config.name.clone(), Arc::new(client));
                }
                Err(e) => {
                    tracing::warn!(
                        account = %config.name,
                        error = %e,
                        "Skipping account, connection failed"
                    );
                }
            }
        }

        if accounts.is_empty() {
            return Err(AppError::Config("No accounts could be connected".into()));
        }

        let default = match default_account {
            Some(name) if accounts.contains_key(name) => name.to_string(),
            Some(name) => {
                return Err(AppError::Config(format!(
                    "Default account '{name}' could not be connected"
                )));
            }
            None => configs
                .iter()
                .find(|config| accounts.contains_key(&config.name))
                .expect("At least one account connected")
                .name
                .clone(),
        };

        Ok(Self { accounts, default })
    }

    /// Get a client by account name. Returns an error if the account doesn't exist.
    /// This enforces account isolation: only accounts configured at startup are accessible,
    /// and each request is validated against the known account list.
    pub fn get(&self, name: Option<&str>) -> Result<&Arc<JmapClient>, AppError> {
        let name = name.unwrap_or(&self.default);
        self.accounts
            .get(name)
            .ok_or_else(|| AppError::NotFound(format!("Account '{}' not found", name)))
    }

    pub fn list(&self) -> Vec<(&str, &Arc<JmapClient>)> {
        let mut accounts: Vec<_> = self
            .accounts
            .iter()
            .map(|(name, client)| (name.as_str(), client))
            .collect();
        accounts.sort_by_key(|(name, _)| *name);
        accounts
    }

    pub fn default_name(&self) -> &str {
        &self.default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use serde_json::json;

    async fn fixture() -> (String, tokio::task::JoinHandle<()>) {
        fixture_with_set_response(json!({})).await
    }

    async fn fixture_with_set_response(
        set_response: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let session = json!({
            "capabilities": {"urn:ietf:params:jmap:core": {
                "maxSizeUpload": 1000000, "maxConcurrentUpload": 4,
                "maxSizeRequest": 1000000, "maxConcurrentRequests": 4,
                "maxCallsInRequest": 16, "maxObjectsInGet": 100,
                "maxObjectsInSet": 100, "collationAlgorithms": []
            }},
            "accounts": {"test": {"name": "Fixture", "isPersonal": true,
                "isReadOnly": false, "accountCapabilities": {}}},
            "primaryAccounts": {"urn:ietf:params:jmap:mail": "test"},
            "username": "fixture@example.test", "apiUrl": format!("{base}/jmap"),
            "downloadUrl": format!("{base}/download/{{accountId}}/{{blobId}}"),
            "uploadUrl": format!("{base}/upload/{{accountId}}"),
            "eventSourceUrl": format!("{base}/events"), "state": "fixture"
        });
        let app = Router::new()
            .route("/jmap", post(move || { let response = set_response.clone(); async move {
                Json(json!({"methodResponses": [["Email/set", response, "s0"]], "sessionState": "fixture"}))
            }}))
            .fallback(get(move || { let session = session.clone(); async move { Json(session) } }));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, task)
    }

    fn config(name: &str, url: &str) -> AccountConfig {
        AccountConfig {
            name: name.into(),
            url: url.into(),
            username: "fixture@example.test".into(),
            // The fixture ignores credentials. Avoid changing the process environment.
            password_env: "PATH".into(),
        }
    }

    // protects=account isolation; fails_when=hash iteration selects another account;
    // why_new=no existing multi-account tests.
    #[tokio::test]
    async fn default_follows_configuration_order() {
        let (url, task) = fixture().await;
        for _ in 0..16 {
            let manager =
                AccountManager::connect_all(&[config("first", &url), config("second", &url)], None)
                    .await
                    .unwrap();
            assert_eq!(manager.default_name(), "first");
        }
        task.abort();
    }

    // protects=explicit account selection; fails_when=missing default silently falls back.
    #[tokio::test]
    async fn missing_explicit_default_fails_closed() {
        let (url, task) = fixture().await;
        let result =
            AccountManager::connect_all(&[config("available", &url)], Some("missing")).await;
        assert!(
            result.is_err(),
            "An explicit unavailable default must not select a different account"
        );
        task.abort();
    }

    // protects=unique account identity; fails_when=duplicate config overwrites a mailbox.
    #[tokio::test]
    async fn duplicate_account_names_are_rejected() {
        let (url, task) = fixture().await;
        let result =
            AccountManager::connect_all(&[config("same", &url), config("same", &url)], None).await;
        assert!(
            result.is_err(),
            "Duplicate account names must not silently replace each other"
        );
        task.abort();
    }
    // protects=mail retention; fails_when=default configuration permits permanent deletion.
    #[tokio::test]
    async fn permanent_delete_requires_explicit_capability() {
        use crate::config::{Capabilities, NotificationsConfig};
        use crate::mcp::{StalwartMcp, tools::mail::BulkDeleteArgs};
        use rmcp::handler::server::wrapper::Parameters;
        let (url, task) = fixture().await;
        let manager = AccountManager::connect_all(&[config("fixture", &url)], None)
            .await
            .unwrap();
        let server = StalwartMcp::new(
            manager,
            Capabilities::default(),
            NotificationsConfig::default(),
            None,
        );
        let result = server
            .mail_bulk_delete(Parameters(BulkDeleteArgs {
                account: Some("not-configured".into()),
                ids: vec!["never-delete".into()],
            }))
            .await;
        let error = result.expect_err("Deletion must be disabled by default");
        assert!(
            error.message.contains("disabled"),
            "Capability denial must precede account/network access: {error}"
        );
        task.abort();
    }

    // protects=account discoverability in tools-only clients; fails_when=discovery needs resources.
    #[test]
    fn account_discovery_is_available_as_a_tool() {
        let tools = crate::mcp::StalwartMcp::create_tool_router().list_all();
        assert!(tools.iter().any(|tool| tool.name == "mail_list_accounts"));
    }
    // protects=honest mutation results; fails_when=HTTP 200 hides per-object failures.
    #[tokio::test]
    async fn mutation_results_check_each_object() {
        for succeeds in [false, true] {
            let response = if succeeds {
                json!({"accountId":"test", "newState":"after", "updated":{"message":null}, "destroyed":["message"]})
            } else {
                json!({"accountId":"test", "newState":"after", "notUpdated":{"message":{"type":"forbidden"}}, "notDestroyed":{"message":{"type":"forbidden"}}})
            };
            let (url, task) = fixture_with_set_response(response).await;
            let client = JmapClient::connect(&config("fixture", &url)).await.unwrap();
            let ids = vec!["message".to_string()];
            assert_eq!(
                client
                    .set_email_keyword("message", "$seen", true)
                    .await
                    .is_ok(),
                succeeds
            );
            assert_eq!(
                client.move_email("message", "archive").await.is_ok(),
                succeeds
            );
            assert_eq!(
                client.bulk_keyword(&ids, "$seen", true).await.is_ok(),
                succeeds
            );
            assert_eq!(
                client
                    .bulk_keyword_and_move(&ids, "$junk", true, "junk")
                    .await
                    .is_ok(),
                succeeds
            );
            assert_eq!(client.bulk_destroy(&ids).await.is_ok(), succeeds);
            task.abort();
        }
    }
}
