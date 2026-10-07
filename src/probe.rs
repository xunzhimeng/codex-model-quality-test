use crate::store::{now_ms, payload_call};
use gateway_plugin_sdk::{
    call::host::{AuthCredential, AuthGetRequest, AuthRuntimeAccount, HttpRequest, HttpResponse},
    client::HostClient,
};
use serde::Serialize;
use serde_json::{Value, json};

const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const MAX_BODY: usize = 1024 * 1024;

#[derive(Serialize)]
pub struct ProbeResult {
    pub status: String,
    pub detail: String,
    pub metrics: Value,
}

pub fn eligibility(account: &AuthRuntimeAccount) -> Result<(), &'static str> {
    if account.provider_id != "openai" || account.authentication_kind != "oauth" {
        return Err("门票探针仅适用于OpenAI OAuth账号");
    }
    if !account.enabled {
        return Err("账号已停用");
    }
    if account
        .access_token_expires_at_ms
        .is_some_and(|expiry| expiry <= now_ms().try_into().unwrap_or(i64::MAX))
    {
        return Err("访问令牌已过期，请先在宿主刷新账号");
    }
    Ok(())
}

#[derive(Default)]
struct Shot {
    status: u16,
    ticket: String,
    cookies: Vec<String>,
    model: Option<String>,
}

pub async fn run(
    host: &HostClient,
    account: &AuthRuntimeAccount,
    model: &str,
) -> Result<ProbeResult, String> {
    eligibility(account).map_err(str::to_owned)?;
    let credential: AuthCredential = payload_call(
        host,
        "host.auth.get",
        &AuthGetRequest {
            account_id: account.account_id.clone(),
        },
    )
    .await?;
    if credential.credential_revision != account.credential_revision
        || credential.facts.authentication_kind != "oauth"
    {
        return Err("账号凭据已变化，请重新加载账号".into());
    }
    let token = credential
        .facts
        .material
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("账号缺少访问令牌")?;
    let account_id = credential
        .facts
        .upstream_account_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("账号缺少上游账号标识")?;
    let first = shot(host, token, account_id, model, "", &[]).await?;
    if first.ticket.is_empty() {
        return Ok(ProbeResult {
            status: "inconclusive".into(),
            detail: "首轮未返回门票，无法判断".into(),
            metrics: json!({"mint_status":first.status,"ticket_length":0}),
        });
    }
    let second = shot(
        host,
        token,
        account_id,
        model,
        &first.ticket,
        &first.cookies,
    )
    .await?;
    let changed = new_ticket(&first.ticket, &second.ticket);
    Ok(ProbeResult {
        status: if changed { "degraded" } else { "healthy" }.into(),
        detail: if changed {
            "续接返回不同门票，经验判据显示疑似降级"
        } else {
            "续接未返回不同门票，经验判据未发现降级"
        }
        .into(),
        // 门票和路由Cookie仅在本次调用内存中使用，不写历史、不返回页面。
        metrics: json!({"mint_status":first.status,"continue_status":second.status,"ticket_length":first.ticket.len(),"continue_ticket_length":second.ticket.len(),"new_ticket":changed,"reported_model":second.model.or(first.model)}),
    })
}

fn new_ticket(first: &str, second: &str) -> bool {
    !second.is_empty() && second != first
}

async fn shot(
    host: &HostClient,
    token: &str,
    account_id: &str,
    model: &str,
    ticket: &str,
    cookies: &[String],
) -> Result<Shot, String> {
    let mut headers = vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("chatgpt-account-id".into(), account_id.into()),
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "text/event-stream".into()),
        ("openai-beta".into(), "responses=experimental".into()),
        ("originator".into(), "codex_cli_rs".into()),
        (
            "user-agent".into(),
            "codex_cli_rs/0.155.0 (Linux; x86_64) model-quality-test/0.1.0".into(),
        ),
        ("version".into(), "0.155.0".into()),
        ("session_id".into(), uuid::Uuid::new_v4().to_string()),
    ];
    if !ticket.is_empty() {
        headers.push(("x-codex-turn-state".into(), ticket.into()));
    }
    if !cookies.is_empty() {
        headers.push(("cookie".into(), cookies.join("; ")));
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: ENDPOINT.into(),
        headers,
    };
    let body = json!({"model":model,"instructions":"Reply with OK.","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Reply with OK."}]}],"stream":true,"store":false,"parallel_tool_calls":true,"include":["reasoning.encrypted_content"]});
    let reply = host
        .call(
            "host.http.do_stream",
            serde_json::to_value(request).map_err(|_| "探针请求编码失败")?,
            serde_json::to_vec(&body).map_err(|_| "探针正文编码失败")?,
        )
        .await
        .map_err(|_| "探针网络调用未完成，无法判断")?;
    let response: HttpResponse =
        serde_json::from_value(reply.result).map_err(|_| "探针响应格式无效")?;
    let stream = response.stream.as_deref().ok_or("探针未提供响应流")?;
    let result = read_shot(host, &response, stream).await;
    // 失败或超过正文上限时也主动释放流，父调用取消由SDK负责回收。
    if result.is_err() {
        let _ = host
            .call("host.http.stream_close", json!({"stream":stream}), vec![])
            .await;
    }
    result
}

async fn read_shot(
    host: &HostClient,
    response: &HttpResponse,
    stream: &str,
) -> Result<Shot, String> {
    if response.status != 200 {
        return Err(match response.status {
            401 | 403 => "上游拒绝认证，请核对令牌与账号权限",
            429 => "上游限流或额度不足，无法判断",
            _ => "上游未成功响应，无法判断",
        }
        .into());
    }
    let mut body = Vec::new();
    loop {
        let reply = host
            .call(
                "host.http.stream_read",
                json!({"stream":stream,"maximum_bytes":65536}),
                vec![],
            )
            .await
            .map_err(|_| "探针响应流未完整读取，无法判断")?;
        if body.len() + reply.payload.len() > MAX_BODY {
            return Err("探针响应超过上限，无法判断".into());
        }
        body.extend(reply.payload);
        if reply
            .result
            .get("eof")
            .and_then(Value::as_bool)
            .ok_or("探针流响应格式无效")?
        {
            break;
        }
    }
    let model = parse_completion(&body)?;
    let mut out = Shot {
        status: response.status,
        model,
        ..Shot::default()
    };
    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("x-codex-turn-state") {
            if value.len() > 8192 || value.bytes().any(|b| b.is_ascii_control()) {
                return Err("门票头无效，无法判断".into());
            }
            if !out.ticket.is_empty() && out.ticket != value.trim() {
                return Err("上游返回冲突门票，无法判断".into());
            }
            out.ticket = value.trim().into();
        }
        if name.eq_ignore_ascii_case("set-cookie") {
            let pair = value.split(';').next().unwrap_or("").trim();
            if let Some((key, val)) = pair.split_once('=')
                && matches!(key, "__cflb" | "__oailb")
                && val.len() <= 4096
                && !val.bytes().any(|b| b.is_ascii_control())
            {
                out.cookies.push(pair.into());
            }
        }
    }
    Ok(out)
}

pub fn parse_completion(body: &[u8]) -> Result<Option<String>, String> {
    let text = std::str::from_utf8(body).map_err(|_| "探针流编码无效")?;
    let normalized = text.replace("\r\n", "\n");
    if !normalized.ends_with("\n\n") {
        return Err("探针流缺少完整终态，无法判断".into());
    }
    let mut completed = false;
    let mut model = None;
    for event in normalized.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let value: Value = serde_json::from_str(&data).map_err(|_| "探针事件格式无效，无法判断")?;
        if value.get("error").is_some_and(|error| !error.is_null()) {
            return Err("上游流内失败，无法判断".into());
        }
        match value.get("type").and_then(Value::as_str) {
            Some("error" | "response.failed" | "response.incomplete") => {
                return Err("上游流内失败，无法判断".into());
            }
            Some("response.completed") => {
                if value.pointer("/response/status").and_then(Value::as_str) != Some("completed")
                    || value
                        .pointer("/response/error")
                        .is_some_and(|e| !e.is_null())
                {
                    return Err("上游响应未完整完成，无法判断".into());
                }
                completed = true;
                model = value
                    .pointer("/response/model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            _ => {}
        }
    }
    if !completed {
        return Err("探针流缺少完成事件，无法判断".into());
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ticket_verdict() {
        assert!(new_ticket("a", "b"));
        assert!(!new_ticket("a", "a"));
        assert!(!new_ticket("a", ""));
    }
    #[test]
    fn rejects_missing_failed_and_truncated_terminal() {
        assert!(parse_completion(b"data: {\"type\":\"response.created\"}\n\n").is_err());
        assert!(
            parse_completion(
                b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}"
            )
            .is_err()
        );
        assert!(parse_completion(b"data: {\"type\":\"response.failed\"}\n\n").is_err());
        assert!(parse_completion(b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"model\":\"test\"}}\r\n\r\n").is_ok());
    }
}
