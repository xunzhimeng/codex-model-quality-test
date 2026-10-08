use crate::{
    accounts,
    store::{metadata_call, payload_call},
};
use gateway_plugin_sdk::{
    call::{
        data::{ClientKeyFacts, ClientKeyFactsQuery},
        host::{ModelListRequest, ModelListResult},
    },
    client::HostClient,
};
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Deserialize)]
struct Group {
    id: String,
    enabled: bool,
}
#[derive(Deserialize)]
struct Page {
    items: Vec<Group>,
    page: Meta,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    total_pages: u32,
}

// Key仅限定范围，不冒充Core计费准入；每次探针均重新核对当前事实。
pub async fn check(
    host: &HostClient,
    key: &str,
    model: &str,
    info: &accounts::AccountInfo,
) -> Result<(), String> {
    if !info.enabled {
        return Err("账号已停用".into());
    }
    let facts: ClientKeyFacts = payload_call(
        host,
        "host.data.keys.get",
        &ClientKeyFactsQuery {
            client_key_id: key.into(),
        },
    )
    .await?;
    if facts.schema_version != 1 || facts.client_key_id != key || !facts.enabled {
        return Err("客户端Key不可用".into());
    }
    let models: ModelListResult = metadata_call(
        host,
        "host.models.list",
        &ModelListRequest {
            client_key_id: key.into(),
            protocol: "openai".into(),
            client_version: concat!("model-quality-test/", env!("CARGO_PKG_VERSION")).into(),
        },
    )
    .await?;
    if !models.models.iter().any(|m| m == model) {
        return Err("模型不在所选Key可见范围".into());
    }
    if !facts.group_ids.is_empty() {
        let bound: BTreeSet<_> = facts.group_ids.iter().map(String::as_str).collect();
        let mut enabled = BTreeSet::new();
        for page in 1..=50 {
            let data: Page = accounts::admin(
                host,
                format!("/api/admin/account-groups?page={page}&pageSize=100"),
                None,
            )
            .await?;
            for group in data.items {
                if group.enabled && bound.contains(group.id.as_str()) {
                    enabled.insert(group.id);
                }
            }
            if page >= data.page.total_pages {
                break;
            }
            if page == 50 {
                return Err("账号组目录超过上限，未继续探针".into());
            }
        }
        if !info.groups.iter().any(|g| enabled.contains(&g.id)) {
            return Err("账号不在Key绑定的启用分组范围".into());
        }
    }
    if !info.model_access.allows(model)? {
        return Err("账号模型政策不允许该探针模型".into());
    }
    Ok(())
}
