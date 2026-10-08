use gateway_plugin_sdk::{
    call::{
        host::AuthRuntimeAccount,
        middleware::{MiddlewareHeader, http::Version},
    },
    client::{HostClient, HttpBody, HttpFrame, HttpRequest},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Deserialize)]
pub struct AccountInfo {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    pub notes: Option<String>,
    pub enabled: bool,
    pub groups: Vec<GroupRef>,
    #[serde(rename = "modelAccess")]
    pub model_access: ModelAccess,
    #[serde(rename = "outboundProxyEndpoint")]
    pub proxy_endpoint: Option<String>,
}
#[derive(Deserialize)]
pub struct GroupRef {
    pub id: String,
}
#[derive(Deserialize)]
pub struct ModelAccess {
    mode: String,
    models: Vec<String>,
}
impl ModelAccess {
    pub fn allows(&self, model: &str) -> Result<bool, String> {
        match self.mode.as_str() {
            "all" if self.models.is_empty() => Ok(true),
            "allowlist" => Ok(self.models.iter().any(|m| m == model)),
            "denylist" => Ok(!self.models.iter().any(|m| m == model)),
            _ => Err("账号模型政策格式无效".into()),
        }
    }
}
#[derive(Deserialize)]
struct Page {
    items: Vec<AccountInfo>,
    page: PageMeta,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageMeta {
    total_pages: u32,
}
#[derive(Deserialize)]
struct Envelope<T> {
    code: u32,
    data: T,
}

// 受管内部HTTP采用宿主插件身份，不保存或转发用户的管理认证头。
pub async fn admin<T: DeserializeOwned>(
    host: &HostClient,
    uri: String,
    body: Option<Value>,
) -> Result<T, String> {
    let request = HttpRequest {
        settings: Value::Null,
        method: if body.is_some() { "POST" } else { "GET" }.into(),
        uri,
        version: Version::Http11,
        timeout_ms: Some(4000),
        headers: vec![MiddlewareHeader {
            name: "content-type".into(),
            value: b"application/json".to_vec(),
        }],
        body: match body {
            Some(value) => {
                HttpBody::from_bytes(serde_json::to_vec(&value).map_err(|_| "账号请求编码失败")?)
            }
            None => HttpBody::empty(),
        },
    };
    let mut response = host
        .dispatch_http(request)
        .await
        .map_err(|_| "宿主账号接口调用未确认")?;
    let result = async {
        if response.status != 200 {
            return Err(format!("宿主账号接口返回HTTP {}", response.status));
        }
        let mut bytes = Vec::new();
        while let Some(frame) = response
            .body
            .read()
            .await
            .map_err(|_| "宿主账号接口正文未完整读取")?
        {
            if let HttpFrame::Data(data) = frame {
                if bytes.len() + data.len() > 2 * 1024 * 1024 {
                    return Err("宿主账号响应超限".into());
                }
                bytes.extend(data);
            }
        }
        let envelope: Envelope<T> =
            serde_json::from_slice(&bytes).map_err(|_| "宿主账号响应格式无效")?;
        if envelope.code != 200 {
            return Err("宿主账号操作未成功".into());
        }
        Ok(envelope.data)
    }
    .await;
    if result.is_err() {
        let _ = response.body.close().await;
    }
    result
}

pub async fn list(host: &HostClient) -> Result<BTreeMap<String, AccountInfo>, String> {
    let mut accounts = BTreeMap::new();
    for page in 1..=50 {
        let data: Page = admin(
            host,
            format!("/api/admin/accounts?page={page}&pageSize=100"),
            None,
        )
        .await?;
        for account in data.items {
            accounts.insert(account.id.clone(), account);
        }
        if page >= data.page.total_pages {
            return Ok(accounts);
        }
    }
    Err("账号超过5000个，无法完整加载".into())
}

pub async fn get(host: &HostClient, id: &str) -> Result<AccountInfo, String> {
    // 账号列表无按ID过滤合同；逐页精确匹配，不误用上游accountId或凭据详情接口。
    for page in 1..=50 {
        let data: Page = admin(
            host,
            format!("/api/admin/accounts?page={page}&pageSize=100"),
            None,
        )
        .await?;
        if let Some(account) = data.items.into_iter().find(|a| a.id == id) {
            return Ok(account);
        }
        if page >= data.page.total_pages {
            break;
        }
    }
    Err("账号不存在或超出目录上限，请刷新列表".into())
}

pub fn label(account: &AuthRuntimeAccount, info: &AccountInfo) -> String {
    let preferred = if account.authentication_kind == "api_key" {
        [
            info.notes.as_deref(),
            Some(info.name.as_str()),
            info.email.as_deref(),
        ]
    } else {
        [
            info.notes.as_deref(),
            info.email.as_deref(),
            Some(info.name.as_str()),
        ]
    };
    preferred
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| {
            !s.is_empty()
                && !matches!(
                    s.to_lowercase().as_str(),
                    "openai 账号" | "openai账号" | "openai 账户" | "openai账户" | "openai account"
                )
        })
        .map(|s| s.chars().take(160).collect())
        .unwrap_or_else(|| {
            format!(
                "账号 {}",
                account.account_id.chars().take(12).collect::<String>()
            )
        })
}

pub async fn disable(host: &HostClient, id: &str) -> Result<(), String> {
    // 只更新enabled，不能回写列表中的代理、模型映射等陈旧字段。
    let _: Value = admin(
        host,
        "/api/admin/accounts/batch-update".into(),
        Some(json!({"accountIds":[id],"enabled":false})),
    )
    .await?;
    if get(host, id).await?.enabled {
        return Err("停用请求已发送，但账号仍启用".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_priority() {
        let account: AuthRuntimeAccount = serde_json::from_value(json!({"account_id":"id-unique","provider_id":"openai","credential_revision":1,"name":"OpenAI 账号","email":"a@example.test","authentication_kind":"oauth","enabled":true,"credential_state":"ready","has_refresh_token":false})).unwrap();
        let mut info = AccountInfo {
            id: account.account_id.clone(),
            name: account.name.clone(),
            email: account.email.clone(),
            notes: Some("主力账号".into()),
            enabled: true,
            groups: vec![],
            model_access: ModelAccess {
                mode: "all".into(),
                models: vec![],
            },
            proxy_endpoint: None,
        };
        assert_eq!(label(&account, &info), "主力账号");
        info.notes = None;
        assert_eq!(label(&account, &info), "a@example.test");
        info.email = None;
        assert_eq!(label(&account, &info), "账号 id-unique");
    }
}
