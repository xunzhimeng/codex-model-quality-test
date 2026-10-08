use crate::{proxy_transport, store::now_ms};
use gateway_plugin_sdk::{call::host::AuthRuntimeAccount, client::HostClient};
use serde::Serialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

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

#[derive(Default, Serialize)]
struct Round {
    phase: String,
    http_status: Option<u16>,
    elapsed_ms: u64,
    completed: bool,
    response_bytes: usize,
    ticket_present: bool,
    ticket_length: usize,
    routing_cookie_count: usize,
    reported_model: Option<String>,
    error: Option<String>,
    #[serde(skip)]
    started: Option<Instant>,
}

pub async fn run(
    host: &HostClient,
    account: &AuthRuntimeAccount,
    model: &str,
    timeout: Duration,
) -> ProbeResult {
    let mut rounds = Vec::new();
    let result = tokio::time::timeout(timeout, execute(host, account, model, &mut rounds)).await;
    let (status, detail, mut metrics) = match result {
        Ok(Ok(result)) => (result.status, result.detail, result.metrics),
        Ok(Err(error)) => ("inconclusive".into(), error, json!({})),
        Err(_) => {
            if let Some(round) = rounds.last_mut() {
                round.error = Some("探针超时，无法判断".into());
                round.elapsed_ms = round
                    .started
                    .map(|t| t.elapsed().as_millis().try_into().unwrap_or(u64::MAX))
                    .unwrap_or(0);
            }
            (
                "inconclusive".into(),
                "探针超时，无法判断，未自动重试".into(),
                json!({}),
            )
        }
    };
    metrics["transport"] = json!("account_proxy");
    metrics["key_billed"] = json!(false);
    metrics["rounds"] = json!(rounds);
    metrics["criterion"] =
        json!("仅在两轮完整成功时比较门票；续接返回不同门票标记疑似降级，不依据模型名称");
    ProbeResult {
        status,
        detail,
        metrics,
    }
}

async fn execute(
    host: &HostClient,
    account: &AuthRuntimeAccount,
    model: &str,
    rounds: &mut Vec<Round>,
) -> Result<ProbeResult, String> {
    eligibility(account).map_err(str::to_owned)?;
    let connection = proxy_transport::connection(
        host,
        &account.account_id,
        account
            .upstream_account_id
            .as_deref()
            .ok_or("账号缺少上游标识")?,
    )
    .await?;
    let client = &connection.client;
    let token = connection.token.as_str();
    let account_id = connection.upstream_id.as_str();
    rounds.push(Round {
        phase: "首轮获取门票".into(),
        ..Round::default()
    });
    let first = measured_shot(
        client,
        token,
        account_id,
        model,
        "",
        &[],
        rounds.last_mut().unwrap(),
    )
    .await?;
    if first.ticket.is_empty() {
        return Ok(ProbeResult {
            status: "inconclusive".into(),
            detail: "首轮未返回门票，无法判断".into(),
            metrics: json!({"mint_status":first.status,"ticket_length":0}),
        });
    }
    rounds.push(Round {
        phase: "携带门票续接".into(),
        ..Round::default()
    });
    let second = measured_shot(
        client,
        token,
        account_id,
        model,
        &first.ticket,
        &first.cookies,
        rounds.last_mut().unwrap(),
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

async fn measured_shot(
    client: &reqwest::Client,
    token: &str,
    account_id: &str,
    model: &str,
    ticket: &str,
    cookies: &[String],
    round: &mut Round,
) -> Result<Shot, String> {
    let start = Instant::now();
    round.started = Some(start);
    let result = shot(client, token, account_id, model, ticket, cookies, round).await;
    round.elapsed_ms = start.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    if let Err(error) = &result {
        round.error = Some(error.clone());
    }
    result
}

async fn shot(
    client: &reqwest::Client,
    token: &str,
    account_id: &str,
    model: &str,
    ticket: &str,
    cookies: &[String],
    round: &mut Round,
) -> Result<Shot, String> {
    let mut headers: Vec<(String, String)> = vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("chatgpt-account-id".into(), account_id.into()),
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "text/event-stream".into()),
        ("openai-beta".into(), "responses=experimental".into()),
        ("originator".into(), "codex_cli_rs".into()),
        (
            "user-agent".into(),
            concat!(
                "codex_cli_rs/0.155.0 (Linux; x86_64) model-quality-test/",
                env!("CARGO_PKG_VERSION")
            )
            .into(),
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
    let body = json!({"model":model,"instructions":"Reply with OK.","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Reply with OK."}]}],"stream":true,"store":false,"parallel_tool_calls":true,"include":["reasoning.encrypted_content"]});
    let mut request = client.post(ENDPOINT);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let mut response = request
        .body(serde_json::to_vec(&body).map_err(|_| "探针正文编码失败")?)
        .send()
        .await
        .map_err(|_| "账号代理出站失败，禁止回退直连")?;
    round.http_status = Some(response.status().as_u16());
    read_shot(&mut response, round).await
}

async fn read_shot(response: &mut reqwest::Response, round: &mut Round) -> Result<Shot, String> {
    let mut out = Shot {
        status: response.status().as_u16(),
        ..Shot::default()
    };
    for (name, value) in response.headers() {
        let name = name.as_str();
        if !name.eq_ignore_ascii_case("x-codex-turn-state")
            && !name.eq_ignore_ascii_case("set-cookie")
        {
            continue;
        }
        let value = value.to_str().map_err(|_| "探针响应头编码无效")?;
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
    round.ticket_present = !out.ticket.is_empty();
    round.ticket_length = out.ticket.len();
    round.routing_cookie_count = out.cookies.len();
    if response.status().as_u16() != 200 {
        return Err(match response.status().as_u16() {
            401 | 403 => "上游拒绝认证，请核对令牌与账号权限",
            429 => "上游限流或额度不足，无法判断",
            _ => "上游未成功响应，无法判断",
        }
        .into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "代理响应流未完整读取，无法判断")?
    {
        if body.len() + chunk.len() > MAX_BODY {
            return Err("探针响应超过上限，无法判断".into());
        }
        body.extend(chunk);
        round.response_bytes = body.len();
    }
    out.model = parse_completion(&body)?;
    round.completed = true;
    round.reported_model = out.model.clone();
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
                if model.as_ref().is_some_and(|name| name.len() > 256) {
                    return Err("回报模型名称超限，无法判断".into());
                }
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
