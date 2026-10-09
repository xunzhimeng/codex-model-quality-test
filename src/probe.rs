use crate::{proxy_transport, store::now_ms};
use gateway_plugin_sdk::{
    call::{host::AuthRuntimeAccount, services::settings::PreviewClientProfile},
    client::HostClient,
};
use reqwest::header::{HeaderMap, HeaderValue};
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
    // 两轮冻结同一份全局画像，下次探针再读取，不复制宿主的版本解析或默认值。
    let profile = host
        .service::<PreviewClientProfile>(("openai".into(), None))
        .await
        .map_err(|_| "宿主OpenAI客户端身份读取失败，未发送探针")?;
    let mut headers = identity_headers(&profile)?;
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
    let mut authorization = HeaderValue::from_str(&format!("Bearer {}", connection.token))
        .map_err(|_| "账号访问令牌无法用于请求头，未发送探针")?;
    authorization.set_sensitive(true);
    headers.insert("authorization", authorization);
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_str(&connection.upstream_id)
            .map_err(|_| "账号上游标识无法用于请求头，未发送探针")?,
    );
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("accept", HeaderValue::from_static("text/event-stream"));
    rounds.push(Round {
        phase: "首轮获取门票".into(),
        ..Round::default()
    });
    let first = measured_shot(client, &headers, model, "", &[], rounds.last_mut().unwrap()).await?;
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
        &headers,
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

fn identity_headers(profile: &serde_json::Map<String, Value>) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for (name, field) in [
        ("originator", "originator"),
        ("version", "codexVersion"),
        ("user-agent", "userAgent"),
    ] {
        let value = profile
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty() && value.len() <= 4096)
            .ok_or("宿主OpenAI客户端身份字段无效，未发送探针")?;
        headers.insert(
            name,
            HeaderValue::from_str(value).map_err(|_| "宿主OpenAI客户端身份头无效，未发送探针")?,
        );
    }
    Ok(headers)
}

fn new_ticket(first: &str, second: &str) -> bool {
    !second.is_empty() && second != first
}

async fn measured_shot(
    client: &reqwest::Client,
    headers: &HeaderMap,
    model: &str,
    ticket: &str,
    cookies: &[String],
    round: &mut Round,
) -> Result<Shot, String> {
    let start = Instant::now();
    round.started = Some(start);
    let result = shot(client, headers, model, ticket, cookies, round).await;
    round.elapsed_ms = start.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    if let Err(error) = &result {
        round.error = Some(error.clone());
    }
    result
}

async fn shot(
    client: &reqwest::Client,
    headers: &HeaderMap,
    model: &str,
    ticket: &str,
    cookies: &[String],
    round: &mut Round,
) -> Result<Shot, String> {
    // HTTP路径与宿主保持同名会话和路由头，每轮使用新的会话，不附加插件身份后缀。
    let mut request = client
        .post(ENDPOINT)
        .headers(headers.clone())
        .header("session-id", uuid::Uuid::new_v4().to_string())
        .header("x-client-request-id", uuid::Uuid::new_v4().to_string())
        .header("x-codex-routing-hint", format!("model={model}"));
    if !ticket.is_empty() {
        request = request.header("x-codex-turn-state", ticket);
    }
    if !cookies.is_empty() {
        request = request.header("cookie", cookies.join("; "));
    }
    let body = json!({"model":model,"instructions":"Reply with OK.","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Reply with OK."}]}],"stream":true,"store":false,"parallel_tool_calls":true,"include":["reasoning.encrypted_content"]});
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
        let status = response.status().as_u16();
        // 错误正文只在内存识别已知原因，不持久化上游原文，避免回显敏感内容。
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| format!("上游拒绝探针请求（HTTP {status}），错误正文读取失败"))?
        {
            if body.len() + chunk.len() > 8192 {
                return Err(format!(
                    "上游拒绝探针请求（HTTP {status}），错误正文超过上限"
                ));
            }
            body.extend(chunk);
            round.response_bytes = body.len();
        }
        let reason = rejection_reason(&body).unwrap_or(match status {
            401 | 403 => "认证或账号权限被拒绝，请在宿主核对账号",
            429 => "上游限流或额度不足",
            400 => "请求被拒绝，未识别具体原因，请核对上游模型及请求参数",
            300..=399 => "上游要求重定向，探针禁止跟随",
            _ => "上游未成功响应",
        });
        return Err(format!("{reason}（HTTP {status}），无法判断"));
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

fn rejection_reason(body: &[u8]) -> Option<&'static str> {
    // 沿用宿主的错误封装顺序，只提取分类字段，不返回或持久化上游原文。
    let value: Value = serde_json::from_slice(body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()));
    let error = [
        value.pointer("/response/error"),
        value.get("error"),
        value.get("detail"),
    ]
    .into_iter()
    .flatten()
    .find(|error| !error.is_null())
    .unwrap_or(&value);
    let field = |name: &str| {
        error
            .get(name)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .or_else(|| value.get(name).and_then(Value::as_str))
            .unwrap_or("")
    };
    let code = field("code");
    let param = field("param");
    let message = field("message");
    let message = if message.is_empty() {
        error.as_str().unwrap_or("")
    } else {
        message
    }
    .to_ascii_lowercase();
    // 结构化字段比文案关键词可靠，参数错误的说明也可能包含“this model”。
    if matches!(code, "unsupported_parameter" | "unsupported_value")
        || matches!(
            param,
            "parallel_tool_calls" | "reasoning.effort" | "include"
        )
    {
        Some("上游不支持探针请求中的参数")
    } else if matches!(code, "invalid_api_key" | "token_expired" | "invalid_token") {
        Some("访问令牌无效或已过期，请先在宿主刷新账号")
    } else if matches!(code, "rate_limit_exceeded" | "insufficient_quota") {
        Some("上游限流或额度不足")
    } else if matches!(
        code,
        "model_not_found" | "unsupported_model" | "model_not_supported"
    ) || (message.contains("model")
        && [
            "not supported",
            "not available",
            "does not exist",
            "not found",
            "do not have access",
        ]
        .iter()
        .any(|text| message.contains(text)))
    {
        Some("上游不支持该模型或账号无权使用，请选择该OAuth账号可用的上游模型")
    } else {
        None
    }
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
    fn recognizes_error_envelopes_without_returning_upstream_text() {
        let message = "The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account fixture-secret";
        for value in [
            json!({"detail":message}),
            json!({"detail":{"message":message}}),
            json!({"error":{"message":message}}),
            json!({"error":message}),
            json!({"response":{"error":{"message":message}}}),
            json!({"message":message}),
            json!(message),
            json!({"error":null,"detail":message}),
        ] {
            let body = serde_json::to_vec(&value).unwrap();
            let reason = rejection_reason(&body).unwrap();
            assert!(reason.starts_with("上游不支持该模型"));
            assert!(!reason.contains("fixture-secret"));
        }
        assert!(
            rejection_reason(message.as_bytes())
                .unwrap()
                .starts_with("上游不支持该模型")
        );
        for body in [
            b"fixture-secret unknown failure".as_slice(),
            b"{\"detail\":\"fixture-secret unknown failure\"}",
            b"{\"detail\":null}",
        ] {
            assert_eq!(rejection_reason(body), None);
        }
    }
    #[test]
    fn nested_structured_rejection_takes_priority_and_uses_root_fields() {
        for value in [
            json!({"detail":{"code":"unsupported_parameter","message":"not supported for this model"}}),
            json!({"response":{"error":{"code":"unsupported_parameter","message":"not supported for this model"}}}),
            json!({"code":"unsupported_parameter","detail":"not supported for this model"}),
            json!({"code":"unsupported_parameter","error":{"code":null,"message":"not supported for this model"}}),
        ] {
            assert_eq!(
                rejection_reason(&serde_json::to_vec(&value).unwrap()),
                Some("上游不支持探针请求中的参数")
            );
        }
        let value = json!({"response":{"error":{"code":"token_expired"}},"error":{"code":"model_not_found"}});
        assert_eq!(
            rejection_reason(&serde_json::to_vec(&value).unwrap()),
            Some("访问令牌无效或已过期，请先在宿主刷新账号")
        );
    }
    #[test]
    fn structured_rejection_takes_priority_over_model_wording() {
        for (code, param, expected) in [
            (
                "unsupported_parameter",
                "parallel_tool_calls",
                "上游不支持探针请求中的参数",
            ),
            (
                "unsupported_value",
                "reasoning.effort",
                "上游不支持探针请求中的参数",
            ),
            ("rate_limit_exceeded", "", "上游限流或额度不足"),
        ] {
            let body = serde_json::to_vec(&json!({"error":{"code":code,"param":param,"message":"not supported for this model"}})).unwrap();
            assert_eq!(rejection_reason(&body), Some(expected));
        }
    }
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
