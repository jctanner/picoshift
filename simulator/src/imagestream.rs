use std::sync::Arc;

use futures::StreamExt;
use kube::api::{Api, DynamicObject, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::watcher;
use kube::runtime::Controller;
use kube::{Client, ResourceExt};
use tracing::{info, warn};

use crate::util::imagestream_ar;

fn status_needs_update(is: &DynamicObject) -> bool {
    let spec_tags = is
        .data
        .get("spec")
        .and_then(|s| s.get("tags"))
        .and_then(|t| t.as_array());

    let status_tags = is
        .data
        .get("status")
        .and_then(|s| s.get("tags"))
        .and_then(|t| t.as_array());

    let spec_tags = match spec_tags {
        Some(t) if !t.is_empty() => t,
        _ => return false,
    };

    let status_tags = match status_tags {
        Some(t) => t,
        None => return true,
    };

    if spec_tags.len() != status_tags.len() {
        return true;
    }

    for spec_tag in spec_tags {
        let tag_name = spec_tag.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let spec_image = spec_tag
            .get("from")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");

        let found = status_tags.iter().any(|st| {
            let st_name = st.get("tag").and_then(|t| t.as_str()).unwrap_or("");
            let st_image = st
                .get("items")
                .and_then(|i| i.as_array())
                .and_then(|arr| arr.first())
                .and_then(|item| item.get("dockerImageReference"))
                .and_then(|d| d.as_str())
                .unwrap_or("");
            st_name == tag_name && st_image == spec_image
        });

        if !found {
            return true;
        }
    }

    false
}

async fn reconcile_imagestream(
    is: Arc<DynamicObject>,
    ctx: Arc<Client>,
) -> Result<Action, kube::Error> {
    let ns = is.namespace().unwrap_or_default();
    let name = is.name_any();

    if !status_needs_update(&is) {
        return Ok(Action::await_change());
    }

    let spec_tags = match is
        .data
        .get("spec")
        .and_then(|s| s.get("tags"))
        .and_then(|t| t.as_array())
    {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(Action::await_change()),
    };

    let now = chrono::Utc::now().to_rfc3339();

    let status_tags: Vec<serde_json::Value> = spec_tags
        .iter()
        .filter_map(|tag| {
            let tag_name = tag.get("name")?.as_str()?;
            let from = tag.get("from")?;
            let image_ref = from.get("name")?.as_str()?;

            Some(serde_json::json!({
                "tag": tag_name,
                "items": [{
                    "dockerImageReference": image_ref,
                    "created": now,
                    "generation": 1,
                    "image": ""
                }]
            }))
        })
        .collect();

    let tag_count = status_tags.len();

    let ar = imagestream_ar();
    let api: Api<DynamicObject> = Api::namespaced_with(ctx.as_ref().clone(), &ns, &ar);

    let status_patch = serde_json::json!({
        "status": {
            "dockerImageRepository": "",
            "tags": status_tags
        }
    });

    api.patch_status(
        &name,
        &PatchParams::apply("ocp-sim"),
        &Patch::Merge(&status_patch),
    )
    .await?;

    info!(ns, name, tag_count, "populated imagestream status");

    Ok(Action::await_change())
}

fn error_policy(
    _obj: Arc<DynamicObject>,
    _error: &kube::Error,
    _ctx: Arc<Client>,
) -> Action {
    Action::requeue(std::time::Duration::from_secs(30))
}

pub async fn run(client: Client) -> Result<(), kube::Error> {
    let ar = imagestream_ar();
    let imagestreams: Api<DynamicObject> = Api::all_with(client.clone(), &ar);
    let ctx = Arc::new(client);

    info!("starting imagestream import controller");

    Controller::new_with(imagestreams, watcher::Config::default(), ar)
        .run(reconcile_imagestream, error_policy, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!("imagestream reconcile error: {e:?}");
            }
        })
        .await;

    Ok(())
}
