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
const MAX_ATTEMPTS: u32 = 3;

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

// 只保存可控分类与已知错误码，不回显上游正文、令牌、门票、Cookie或代理认证。
#[derive(Clone, Debug, Serialize)]
struct Failure {
    kind: &'static str,
    message: String,
    http_status: Option<u16>,
    code: Option<&'static str>,
    retryable: bool,
    retry_after_ms: Option<u64>,
}
impl Failure {
    fn new(kind: &'static str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            kind,
            message: message.into(),
            http_status: None,
            code: None,
            retryable,
            retry_after_ms: None,
        }
    }
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::new("preparation", message, false)
    }
}
impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::new("preparation", message, false)
    }
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
    attempt: u32,
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
    #[serde(skip)]
    failure: Option<Failure>,
}

pub async fn run(
    host: &HostClient,
    account: &AuthRuntimeAccount,
    model: &str,
    timeout: Duration,
    proxy_required: bool,
) -> ProbeResult {
    let start = Instant::now();
    let attempt_budget = timeout / MAX_ATTEMPTS;
    let mut rounds = Vec::new();
    let mut attempts = Vec::new();
    let mut stop = None;
    let mut transport = None;
    // 一次测试冻结身份与代理，重试从新的首轮开始，绝不复用失败尝试的门票。
    let prepared =
        tokio::time::timeout(attempt_budget, prepare(host, account, proxy_required)).await;
    let mut final_result = match prepared {
        Ok(Ok((connection, headers))) => {
            transport = Some(connection.transport);
            let mut result = Err(Failure::new("timeout", "探针超时", true));
            for number in 1..=MAX_ATTEMPTS {
                let remaining = timeout.saturating_sub(start.elapsed());
                if remaining.is_zero() {
                    stop = Some("budget_exhausted");
                    break;
                }
                let attempt_start = Instant::now();
                result = match tokio::time::timeout(
                    attempt_budget.min(remaining),
                    execute(&connection.client, &headers, model, number, &mut rounds),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        let mut failure = Failure::new("timeout", "探针超时", true);
                        if let Some(round) = rounds.last_mut().filter(|r| r.attempt == number) {
                            // 已收到HTTP拒绝时保留状态与Retry-After，正文超时不能绕过限流等待。
                            if let Some(known) = round.failure.take() {
                                failure = known;
                            }
                            round.error = Some(failure.message.clone());
                            round.elapsed_ms = round.started.map(elapsed_ms).unwrap_or(0);
                        }
                        Err(failure)
                    }
                };
                attempts.push(json!({"number":number,"status":if result.is_ok(){"completed"}else{"failed"},"elapsed_ms":elapsed_ms(attempt_start),"failure":result.as_ref().err()}));
                let Err(failure) = &result else { break };
                let remaining = timeout.saturating_sub(start.elapsed());
                match retry_wait(failure, number, remaining) {
                    Ok(wait) => tokio::time::sleep(wait).await,
                    Err(reason) => {
                        stop = Some(reason);
                        break;
                    }
                }
            }
            result
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Err(Failure::new(
            "preparation",
            "探针准备超时，未发送上游请求",
            false,
        )),
    };
    if attempts.is_empty() {
        let failure = final_result.as_ref().err();
        attempts.push(
            json!({"number":1,"status":"failed","elapsed_ms":elapsed_ms(start),"failure":failure}),
        );
        stop = Some("not_retryable");
    }
    let (status, detail, mut metrics) = match &mut final_result {
        Ok(result) => (
            result.status.clone(),
            result.detail.clone(),
            result.metrics.take(),
        ),
        Err(error) => (
            "inconclusive".into(),
            error.message.clone(),
            json!({"failure":error}),
        ),
    };
    if let Some(transport) = transport {
        metrics["transport"] = json!(transport);
    }
    metrics["key_billed"] = json!(false);
    metrics["rounds"] = json!(rounds);
    metrics["attempt_count"] = json!(attempts.len());
    metrics["attempts"] = json!(attempts);
    metrics["retry_stop"] = json!(stop);
    metrics["criterion"] = json!(
        "仅在同一次尝试的两轮完整成功时比较门票；续接返回不同门票标记疑似降级，不依据模型名称"
    );
    ProbeResult {
        status,
        detail,
        metrics,
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

fn retry_wait(
    failure: &Failure,
    number: u32,
    remaining: Duration,
) -> Result<Duration, &'static str> {
    if !failure.retryable {
        return Err("not_retryable");
    }
    if number >= MAX_ATTEMPTS {
        return Err("max_attempts");
    }
    let wait = Duration::from_millis(failure.retry_after_ms.unwrap_or(250 * u64::from(number)));
    // 保留至少一点执行预算，不能等待完后再发送注定被立即取消的请求。
    if wait.saturating_add(Duration::from_millis(100)) >= remaining {
        return Err(if failure.retry_after_ms.is_some() {
            "retry_after_exceeds_budget"
        } else {
            "budget_exhausted"
        });
    }
    Ok(wait)
}

async fn prepare(
    host: &HostClient,
    account: &AuthRuntimeAccount,
    proxy_required: bool,
) -> Result<(proxy_transport::Connection, HeaderMap), Failure> {
    eligibility(account)?;
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
        proxy_required,
    )
    .await?;
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
    Ok((connection, headers))
}

async fn execute(
    client: &reqwest::Client,
    headers: &HeaderMap,
    model: &str,
    attempt: u32,
    rounds: &mut Vec<Round>,
) -> Result<ProbeResult, Failure> {
    rounds.push(Round {
        attempt,
        phase: "首轮获取门票".into(),
        ..Round::default()
    });
    let first = measured_shot(client, headers, model, "", &[], rounds.last_mut().unwrap()).await?;
    if first.ticket.is_empty() {
        return Err(Failure::new(
            "missing_ticket",
            "首轮未返回门票，未取得降级判定",
            true,
        ));
    }
    rounds.push(Round {
        attempt,
        phase: "携带门票续接".into(),
        ..Round::default()
    });
    let second = measured_shot(
        client,
        headers,
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
) -> Result<Shot, Failure> {
    let start = Instant::now();
    round.started = Some(start);
    let result = shot(client, headers, model, ticket, cookies, round).await;
    round.elapsed_ms = elapsed_ms(start);
    if let Err(error) = &result {
        round.error = Some(error.message.clone());
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
) -> Result<Shot, Failure> {
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
        .map_err(|error| {
            if error.is_timeout() {
                Failure::new("timeout", "探针请求超时", true)
            } else {
                Failure::new("connection_error", "探针连接失败", true)
            }
        })?;
    round.http_status = Some(response.status().as_u16());
    read_shot(&mut response, round).await
}

async fn read_shot(response: &mut reqwest::Response, round: &mut Round) -> Result<Shot, Failure> {
    let mut out = Shot {
        status: response.status().as_u16(),
        ..Shot::default()
    };
    let status = out.status;
    if status != 200 {
        let retry_after = response.headers().get("retry-after").map(|v| {
            // 有等待声明但无法解析时不猜测短等待，保守停止本轮重试。
            v.to_str()
                .ok()
                .and_then(|s| retry_after_ms(s, now_ms()))
                .unwrap_or(u64::MAX)
        });
        round.failure = Some(http_failure(status, &[], retry_after));
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| http_failure(status, &body, retry_after))?
        {
            if body.len() + chunk.len() > 8192 {
                break;
            }
            body.extend(chunk);
            round.response_bytes = body.len();
        }
        return Err(http_failure(status, &body, retry_after));
    }
    for (name, value) in response.headers() {
        let name = name.as_str();
        if !name.eq_ignore_ascii_case("x-codex-turn-state")
            && !name.eq_ignore_ascii_case("set-cookie")
        {
            continue;
        }
        let value = value
            .to_str()
            .map_err(|_| Failure::new("invalid_response", "探针响应头编码无效", false))?;
        if name.eq_ignore_ascii_case("x-codex-turn-state") {
            if value.len() > 8192 || value.bytes().any(|b| b.is_ascii_control()) {
                return Err(Failure::new("invalid_response", "门票头无效", false));
            }
            if !out.ticket.is_empty() && out.ticket != value.trim() {
                return Err(Failure::new("invalid_response", "上游返回冲突门票", false));
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
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Failure::new("read_error", "响应流读取中断", true))?
    {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(Failure::new("invalid_response", "探针响应超过上限", false));
        }
        body.extend(chunk);
        round.response_bytes = body.len();
    }
    out.model = parse_completion(&body)?;
    round.completed = true;
    round.reported_model = out.model.clone();
    Ok(out)
}

fn http_failure(status: u16, body: &[u8], retry_after: Option<u64>) -> Failure {
    let value: Value = serde_json::from_slice(body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()));
    let mut failure = classified_error(&value).unwrap_or_else(|| match status {
        401 => Failure::new("authentication", "认证失败，请在宿主刷新账号", false),
        403 => Failure::new("permission_denied", "账号权限被拒绝", false),
        429 => Failure::new("rate_limited", "上游限流", true),
        408 => Failure::new("timeout", "上游请求超时", true),
        500..=599 => Failure::new("upstream_error", "上游服务错误", true),
        300..=399 => Failure::new("invalid_request", "上游要求重定向，探针禁止跟随", false),
        _ => Failure::new("invalid_request", "上游拒绝探针请求，未识别具体原因", false),
    });
    failure.http_status = Some(status);
    if retry_after.is_some() {
        failure.retry_after_ms = retry_after;
    }
    // HTTP认证失败本身已足够确定不可重试，不能被含糊的错误文案放宽。
    if matches!(status, 401 | 403) {
        failure.retryable = false;
    }
    if retry_after == Some(u64::MAX) {
        failure.retry_after_ms = None;
        failure.retryable = false;
        failure
            .message
            .push_str("，Retry-After无效或过大，停止自动重试");
    }
    failure.message = format!("{}（HTTP {status}）", failure.message);
    failure
}

fn classified_error(value: &Value) -> Option<Failure> {
    let error = [
        value.pointer("/response/error"),
        value.get("error"),
        value.get("detail"),
    ]
    .into_iter()
    .flatten()
    .find(|error| !error.is_null())
    .unwrap_or(value);
    let field = |name: &str| {
        error
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| value.get(name).and_then(Value::as_str))
            .unwrap_or("")
    };
    let code = if field("code").is_empty() {
        field("type")
    } else {
        field("code")
    };
    let param = field("param");
    let message = field("message");
    let message = if message.is_empty() {
        error.as_str().unwrap_or("")
    } else {
        message
    }
    .to_ascii_lowercase();
    let (kind, text, retryable) = if matches!(code, "unsupported_parameter" | "unsupported_value")
        || matches!(
            param,
            "parallel_tool_calls" | "reasoning.effort" | "include"
        ) {
        ("invalid_request", "上游不支持探针请求中的参数", false)
    } else if matches!(
        code,
        "invalid_api_key" | "token_expired" | "invalid_token" | "account_deactivated"
    ) {
        (
            "authentication",
            "访问令牌无效或已过期，请先在宿主刷新账号",
            false,
        )
    } else if code == "insufficient_quota" || code == "usage_limit_reached" {
        ("quota_exhausted", "账号额度已耗尽", false)
    } else if matches!(code, "rate_limit_exceeded" | "turn_rate_limit_exceeded") {
        ("rate_limited", "上游限流", true)
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
        .any(|s| message.contains(s)))
    {
        (
            "model_unavailable",
            "上游不支持该模型或账号无权使用，请选择该OAuth账号可用的上游模型",
            false,
        )
    } else if matches!(
        code,
        "server_error" | "internal_error" | "overloaded" | "temporarily_unavailable"
    ) {
        ("upstream_error", "上游服务暂时不可用", true)
    } else {
        return None;
    };
    let mut failure = Failure::new(kind, text, retryable);
    failure.code = match code {
        "unsupported_parameter" => Some("unsupported_parameter"),
        "unsupported_value" => Some("unsupported_value"),
        "invalid_api_key" => Some("invalid_api_key"),
        "token_expired" => Some("token_expired"),
        "invalid_token" => Some("invalid_token"),
        "account_deactivated" => Some("account_deactivated"),
        "insufficient_quota" => Some("insufficient_quota"),
        "usage_limit_reached" => Some("usage_limit_reached"),
        "rate_limit_exceeded" => Some("rate_limit_exceeded"),
        "turn_rate_limit_exceeded" => Some("turn_rate_limit_exceeded"),
        "model_not_found" => Some("model_not_found"),
        "unsupported_model" => Some("unsupported_model"),
        "model_not_supported" => Some("model_not_supported"),
        "server_error" => Some("server_error"),
        "internal_error" => Some("internal_error"),
        "overloaded" => Some("overloaded"),
        "temporarily_unavailable" => Some("temporarily_unavailable"),
        _ => None,
    };
    // 流内错误也可能携带Retry-After，不能因为HTTP是200而忽略等待建议。
    let retry_header = error
        .get("headers")
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        })
        .map(|(_, value)| value);
    if let Some(header) = retry_header {
        let wait = header
            .as_str()
            .and_then(|s| retry_after_ms(s, now_ms()))
            .or_else(|| header.as_u64().map(|s| s.saturating_mul(1000)));
        if let Some(wait) = wait.filter(|wait| *wait < u64::MAX) {
            failure.retry_after_ms = Some(wait);
        } else {
            failure.retryable = false;
            failure
                .message
                .push_str("，Retry-After无效或过大，停止自动重试");
        }
    }
    Some(failure)
}

// Retry-After仅接收整数秒或HTTP标准IMF-fixdate；无效值由调用方保守处理。
fn retry_after_ms(value: &str, now: u64) -> Option<u64> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return Some(
            value
                .parse::<u64>()
                .unwrap_or(u64::MAX)
                .saturating_mul(1000),
        );
    }
    let parts: Vec<_> = value.split_ascii_whitespace().collect();
    if parts.len() != 6
        || !["Mon,", "Tue,", "Wed,", "Thu,", "Fri,", "Sat,", "Sun,"].contains(&parts[0])
        || parts[5] != "GMT"
    {
        return None;
    }
    let day = parts[1].parse::<u64>().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == parts[2])?;
    let year = parts[3].parse::<u64>().ok()?;
    if !(1970..=9999).contains(&year) {
        return None;
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let lengths = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > lengths[month] {
        return None;
    }
    let clock: Vec<u64> = parts[4]
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if clock.len() != 3 || clock[0] > 23 || clock[1] > 59 || clock[2] > 59 {
        return None;
    }
    let previous = year - 1;
    let days = (year - 1970) * 365 + previous / 4 - 1969 / 4 - (previous / 100 - 1969 / 100)
        + previous / 400
        - 1969 / 400
        + lengths[..month].iter().sum::<u64>()
        + day
        - 1;
    let date = (days * 86400 + clock[0] * 3600 + clock[1] * 60 + clock[2]) * 1000;
    Some(date.saturating_sub(now))
}

fn parse_completion(body: &[u8]) -> Result<Option<String>, Failure> {
    let text = std::str::from_utf8(body)
        .map_err(|_| Failure::new("invalid_response", "探针流编码无效", false))?;
    let normalized = text.replace("\r\n", "\n");
    if !normalized.ends_with("\n\n") {
        return Err(Failure::new("read_error", "探针流缺少完整终态", true));
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
        let value: Value = serde_json::from_str(&data)
            .map_err(|_| Failure::new("invalid_response", "探针事件格式无效", false))?;
        let event_type = value.get("type").and_then(Value::as_str);
        if value.get("error").is_some_and(|e| !e.is_null())
            || matches!(
                event_type,
                Some("error" | "response.failed" | "response.incomplete")
            )
            || value
                .pointer("/response/error")
                .is_some_and(|e| !e.is_null())
        {
            let error_status = [
                value.pointer("/response/error/status_code"),
                value.pointer("/error/status_code"),
                value.get("status_code"),
                value.get("status"),
            ]
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .find(|status| (400..=599).contains(status));
            if let Some(status) = error_status {
                return Err(http_failure(status as u16, data.as_bytes(), None));
            }
            return Err(classified_error(&value)
                .unwrap_or_else(|| Failure::new("stream_error", "上游响应流失败或未完成", true)));
        }
        if event_type == Some("response.completed") {
            if value.pointer("/response/status").and_then(Value::as_str) != Some("completed") {
                return Err(Failure::new("stream_error", "上游响应未完整完成", true));
            }
            completed = true;
            model = value
                .pointer("/response/model")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if model.as_ref().is_some_and(|name| name.len() > 256) {
                return Err(Failure::new("invalid_response", "回报模型名称超限", false));
            }
        }
    }
    if !completed {
        return Err(Failure::new("read_error", "探针流缺少完成事件", true));
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_envelopes_are_classified_without_returning_upstream_text() {
        let message = "The model is not supported fixture-secret";
        for value in [
            json!({"detail":message}),
            json!({"detail":{"message":message}}),
            json!({"error":{"message":message}}),
            json!({"error":message}),
            json!({"response":{"error":{"message":message}}}),
            json!({"message":message}),
            json!(message),
        ] {
            let error = classified_error(&value).unwrap();
            assert_eq!(error.kind, "model_unavailable");
            assert!(!error.retryable);
            assert!(
                !serde_json::to_string(&error)
                    .unwrap()
                    .contains("fixture-secret")
            );
        }
        assert!(classified_error(&json!({"message":"fixture-secret unknown failure"})).is_none());
    }
    #[test]
    fn structured_errors_win_over_model_wording() {
        for (code, kind, retryable) in [
            ("unsupported_parameter", "invalid_request", false),
            ("token_expired", "authentication", false),
            ("insufficient_quota", "quota_exhausted", false),
            ("rate_limit_exceeded", "rate_limited", true),
        ] {
            let error=classified_error(&json!({"response":{"error":{"code":code,"message":"not supported for this model"}}})).unwrap();
            assert_eq!(error.kind, kind);
            assert_eq!(error.retryable, retryable);
        }
        let error = http_failure(403, br#"{"error":{"code":"rate_limit_exceeded"}}"#, None);
        assert!(!error.retryable);
        assert_eq!(error.http_status, Some(403));
    }
    #[test]
    fn retry_after_accepts_seconds_and_http_date() {
        assert_eq!(retry_after_ms("120", 0), Some(120000));
        assert_eq!(
            retry_after_ms("Thu, 01 Jan 1970 00:02:00 GMT", 0),
            Some(120000)
        );
        assert_eq!(
            retry_after_ms("Sun, 06 Nov 1994 08:49:37 GMT", 784111776000),
            Some(1000)
        );
        assert_eq!(retry_after_ms("-1", 0), None);
        assert_eq!(retry_after_ms("Mon, 31 Feb 2025 00:00:00 GMT", 0), None);
    }
    #[test]
    fn retry_is_bounded_and_respects_remaining_budget() {
        let mut error = Failure::new("timeout", "探针超时", true);
        assert!(retry_wait(&error, 1, Duration::from_secs(1)).is_ok());
        assert_eq!(
            retry_wait(&error, 3, Duration::from_secs(10)),
            Err("max_attempts")
        );
        error.retry_after_ms = Some(30000);
        assert_eq!(
            retry_wait(&error, 1, Duration::from_secs(10)),
            Err("retry_after_exceeds_budget")
        );
        error.retryable = false;
        assert_eq!(
            retry_wait(&error, 1, Duration::from_secs(100)),
            Err("not_retryable")
        );
    }
    #[test]
    fn ticket_verdict() {
        assert!(new_ticket("a", "b"));
        assert!(!new_ticket("a", "a"));
        assert!(!new_ticket("a", ""));
    }
    #[test]
    fn sse_error_retry_after_is_preserved() {
        let error=parse_completion(b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"headers\":{\"Retry-After\":\"120\"}}}}\n\n").unwrap_err();
        assert_eq!(error.retry_after_ms, Some(120000));
        let typed = classified_error(&json!({"error":{"type":"insufficient_quota"}})).unwrap();
        assert_eq!(typed.kind, "quota_exhausted");
        assert!(!typed.retryable);
        let status = parse_completion(b"data: {\"type\":\"error\",\"status\":429,\"error\":{\"code\":\"rate_limit_exceeded\"}}\n\n").unwrap_err();
        assert_eq!(status.http_status, Some(429));
        assert!(status.message.contains("HTTP 429"));
        assert_eq!(
            retry_wait(&error, 1, Duration::from_secs(90)),
            Err("retry_after_exceeds_budget")
        );
    }
    #[test]
    fn stream_failures_keep_specific_error_class() {
        let error=parse_completion(b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"insufficient_quota\"}}}\n\n").unwrap_err();
        assert_eq!(error.kind, "quota_exhausted");
        assert!(!error.retryable);
        assert!(
            parse_completion(
                b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}"
            )
            .unwrap_err()
            .retryable
        );
        assert!(parse_completion(b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"model\":\"test\"}}\r\n\r\n").is_ok());
    }
}
