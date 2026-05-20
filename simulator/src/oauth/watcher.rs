use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use kube::api::Api;
use kube::runtime::watcher;
use kube::runtime::WatchStreamExt;
use kube::Client;
use tokio::sync::RwLock;
use tracing::{info, warn};

use super::types::UserStore;

const HTPASSWD_SECRET: &str = "htpass-secret";
const HTPASSWD_NS: &str = "openshift-config";
const HTPASSWD_KEY: &str = "htpasswd";

pub(crate) async fn watch_htpasswd_secret(
    client: Client,
    user_store: Arc<RwLock<UserStore>>,
) {
    info!("starting htpasswd secret watcher ({HTPASSWD_NS}/{HTPASSWD_SECRET})");

    let secrets: Api<Secret> = Api::namespaced(client, HTPASSWD_NS);
    let stream = watcher::watcher(
        secrets,
        watcher::Config::default()
            .fields(&format!("metadata.name={HTPASSWD_SECRET}")),
    )
    .default_backoff()
    .applied_objects();

    tokio::pin!(stream);

    while let Some(event) = stream.next().await {
        match event {
            Ok(secret) => {
                let name = secret.metadata.name.as_deref().unwrap_or("");
                if name != HTPASSWD_SECRET {
                    continue;
                }
                let htpasswd_data = secret
                    .data
                    .as_ref()
                    .and_then(|d| d.get(HTPASSWD_KEY))
                    .map(|b| String::from_utf8_lossy(&b.0).to_string())
                    .or_else(|| {
                        secret
                            .string_data
                            .as_ref()
                            .and_then(|d| d.get(HTPASSWD_KEY).cloned())
                    });

                match htpasswd_data {
                    Some(data) => {
                        let new_store = UserStore::from_htpasswd(&data);
                        let count = new_store.users.len();
                        *user_store.write().await = new_store;
                        info!(count, "reloaded user store from htpasswd secret");
                    }
                    None => {
                        warn!("htpass-secret exists but has no 'htpasswd' key");
                    }
                }
            }
            Err(e) => {
                warn!(%e, "htpasswd secret watch error");
            }
        }
    }
}
