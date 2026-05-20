use kube::api::{Api, DynamicObject, Patch, PatchParams};
use kube::Client;
use tracing::{info, warn};

use crate::util::{oauth_client_ar, user_ar, identity_ar};

pub(crate) async fn validate_client(
    client: &Client,
    client_id: &str,
) -> Option<(String, Vec<String>)> {
    let ar = oauth_client_ar();
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
    let oauth_client = api.get(client_id).await.ok()?;

    let secret = oauth_client
        .data
        .get("secret")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();

    let redirect_uris = oauth_client
        .data
        .get("redirectURIs")
        .and_then(|u| u.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Some((secret, redirect_uris))
}

pub(crate) async fn ensure_user_and_identity(client: &Client, username: &str) {
    let identity_name = format!("ocp-sim.{username}");

    let user_ar = user_ar();
    let users: Api<DynamicObject> = Api::all_with(client.clone(), &user_ar);
    let user_obj = serde_json::json!({
        "apiVersion": "user.openshift.io/v1",
        "kind": "User",
        "metadata": {
            "name": username,
        },
        "fullName": username,
        "identities": [&identity_name],
    });
    match users
        .patch(
            username,
            &PatchParams::apply("ocp-sim-oauth"),
            &Patch::Apply(serde_json::from_value::<DynamicObject>(user_obj).unwrap()),
        )
        .await
    {
        Ok(u) => {
            let uid = u.metadata.uid.as_deref().unwrap_or("unknown");
            info!(username, uid, "ensured User object");

            let id_ar = identity_ar();
            let identities: Api<DynamicObject> = Api::all_with(client.clone(), &id_ar);
            let id_obj = serde_json::json!({
                "apiVersion": "user.openshift.io/v1",
                "kind": "Identity",
                "metadata": {
                    "name": &identity_name,
                },
                "providerName": "ocp-sim",
                "providerUserName": username,
                "user": {
                    "name": username,
                    "uid": uid,
                },
            });
            match identities
                .patch(
                    &identity_name,
                    &PatchParams::apply("ocp-sim-oauth"),
                    &Patch::Apply(serde_json::from_value::<DynamicObject>(id_obj).unwrap()),
                )
                .await
            {
                Ok(_) => info!(identity_name, "ensured Identity object"),
                Err(e) => warn!(identity_name, %e, "failed to create Identity"),
            }
        }
        Err(e) => warn!(username, %e, "failed to create User"),
    }
}
