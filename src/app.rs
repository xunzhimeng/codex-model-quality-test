use crate::{
    probe,
    questions::{self, Question},
    store::{self, CustomBank, History, Record, metadata_call, now_ms, payload_call},
};
use gateway_plugin_sdk::{
    call::{
        host::{
            AuthGetRequest, AuthListRequest, AuthListResult, AuthRuntimeAccount, KeyListRequest,
            KeyListResult, ModelEventBatch, ModelExecuteRequest, ModelListRequest, ModelListResult,
            ModelOperation,
        },
        management::{ManagementRequest, ManagementResponse},
        model::{CanonicalEvent, ExecutionEvent, FinishReason, Usage},
    },
    client::{HostClient, TypedCall, TypedReply},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

type Response = TypedReply<ManagementResponse>;
pub struct App {
    history: AsyncMutex<()>,
    bank: AsyncMutex<()>,
    slots: Arc<Semaphore>,
    accounts: Arc<Mutex<BTreeSet<String>>>,
}
impl Default for App {
    fn default() -> Self {
        Self {
            history: AsyncMutex::new(()),
            bank: AsyncMutex::new(()),
            slots: Arc::new(Semaphore::new(4)),
            accounts: Arc::default(),
        }
    }
}
struct AccountGuard {
    accounts: Arc<Mutex<BTreeSet<String>>>,
    id: String,
}
impl Drop for AccountGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.accounts.lock() {
            set.remove(&self.id);
        }
    }
}

// 取消本地等待不等于上游取消完成，未完成调用继续保留账号与并发槽的隔离窗口。
struct PendingAccount {
    guard: Option<(AccountGuard, tokio::sync::OwnedSemaphorePermit)>,
    completed: bool,
}
impl Drop for PendingAccount {
    fn drop(&mut self) {
        if !self.completed
            && let Some(guard) = self.guard.take()
        {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(150)).await;
                drop(guard);
            });
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    pub id: String,
    pub batch_id: String,
    pub account_id: String,
    pub mode: String,
    pub model: String,
    pub effort: String,
    pub client_key_id: Option<String>,
    pub question_id: Option<String>,
}
impl RunRequest {
    pub fn validate(&self) -> Result<(), String> {
        for id in [&self.id, &self.batch_id] {
            if id.len() < 8
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                return Err("操作标识无效".into());
            }
        }
        if self.account_id.is_empty()
            || self.account_id.len() > 128
            || self.model.trim() != self.model
            || self.model.is_empty()
            || self.model.len() > 128
            || self.model.bytes().any(|b| b.is_ascii_control())
        {
            return Err("账号或模型无效".into());
        }
        if !matches!(
            self.effort.as_str(),
            "default" | "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            return Err("思考强度无效".into());
        }
        if !matches!(self.mode.as_str(), "question" | "probe") {
            return Err("测试类型无效".into());
        }
        if self.mode == "question"
            && (self
                .client_key_id
                .as_ref()
                .is_none_or(|s| s.is_empty() || s.len() > 128)
                || self
                    .question_id
                    .as_ref()
                    .is_none_or(|s| s.is_empty() || s.len() > 64))
        {
            return Err("请选择客户端Key与题目".into());
        }
        if self.mode == "probe"
            && (self.question_id.is_some()
                || self.client_key_id.is_some()
                || self.effort != "default")
        {
            return Err("探针不接受题目、Key或思考强度".into());
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelsRequest {
    client_key_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BankRequest {
    questions: Vec<Question>,
    expected_version: Option<u64>,
}

pub fn json_response(status: u16, value: &impl Serialize) -> Result<Response, String> {
    Ok(TypedReply::new(ManagementResponse {
        status,
        content_type: "application/json".into(),
        headers: vec![],
    })
    .with_payload(serde_json::to_vec(value).map_err(|_| "响应编码失败")?))
}
fn decode<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, String> {
    if payload.len() > 270_000 {
        return Err("请求正文超过限制".into());
    }
    serde_json::from_slice(payload).map_err(|_| "请求字段或JSON格式无效".into())
}

impl App {
    pub async fn handle(&self, call: TypedCall<ManagementRequest>) -> Result<Response, String> {
        let host = &call.host;
        match (call.request.method.as_str(), call.request.path.as_str()) {
            ("GET", "/catalog") => {
                let mut accounts = Vec::new();
                let mut cursor = None;
                loop {
                    let page: AuthListResult = payload_call(
                        host,
                        "host.auth.list",
                        &AuthListRequest {
                            provider_id: None,
                            cursor,
                            limit: 100,
                        },
                    )
                    .await?;
                    accounts.extend(page.accounts.into_iter().map(|a| json!({"id":a.account_id,"name":a.name,"provider":a.provider_id,"authentication_kind":a.authentication_kind,"enabled":a.enabled,"probe_supported":probe::eligibility(&a).is_ok()})));
                    cursor = page.next_cursor;
                    if cursor.is_none() {
                        break;
                    }
                    if accounts.len() >= 5000 {
                        return Err("账号超过5000个，当前页面不支持该规模".into());
                    }
                }
                let mut keys = Vec::new();
                let mut cursor = None;
                loop {
                    let page: KeyListResult = metadata_call(
                        host,
                        "host.keys.list",
                        &KeyListRequest { cursor, limit: 100 },
                    )
                    .await?;
                    keys.extend(page.keys);
                    cursor = page.next_cursor;
                    if cursor.is_none() {
                        break;
                    }
                    if keys.len() >= 5000 {
                        return Err("Key超过5000个，当前页面不支持该规模".into());
                    }
                }
                let (custom, version) = store::get::<CustomBank>(host, "bank").await?;
                let mut bank = questions::builtins();
                bank.extend(custom.questions);
                json_response(
                    200,
                    &json!({"accounts":accounts,"keys":keys,"questions":bank,"bank_version":version,"version":env!("CARGO_PKG_VERSION")}),
                )
            }
            ("POST", "/models") => {
                let request: ModelsRequest = decode(&call.payload)?;
                if request.client_key_id.is_empty() || request.client_key_id.len() > 128 {
                    return Err("客户端Key无效".into());
                }
                let models: ModelListResult = metadata_call(
                    host,
                    "host.models.list",
                    &ModelListRequest {
                        client_key_id: request.client_key_id,
                        protocol: "openai".into(),
                        client_version: concat!("model-quality-test/", env!("CARGO_PKG_VERSION"))
                            .into(),
                    },
                )
                .await?;
                json_response(200, &models)
            }
            ("GET", "/history") => {
                let (history, _) = store::get::<History>(host, "history").await?;
                json_response(200, &history)
            }
            ("POST", "/questions") => {
                let request: BankRequest = decode(&call.payload)?;
                questions::validate_custom(&request.questions).map_err(str::to_owned)?;
                let _guard = self.bank.lock().await;
                let (_, version) = store::get::<CustomBank>(host, "bank").await?;
                if version != request.expected_version {
                    return json_response(409, &json!({"error":"题库已变化，请刷新后再保存"}));
                }
                store::put(
                    host,
                    "bank",
                    &CustomBank {
                        questions: request.questions,
                    },
                    version,
                )
                .await?;
                json_response(200, &json!({"saved":true}))
            }
            ("POST", "/run") => {
                let request: RunRequest = decode(&call.payload)?;
                request.validate()?;
                self.run(&call, request).await
            }
            _ => json_response(404, &json!({"error":"请求未注册"})),
        }
    }

    async fn run(
        &self,
        call: &TypedCall<ManagementRequest>,
        request: RunRequest,
    ) -> Result<Response, String> {
        let host = &call.host;
        let _slot = match self.slots.clone().try_acquire_owned() {
            Ok(slot) => slot,
            Err(_) => return json_response(429, &json!({"error":"最多同时运行4个测试"})),
        };
        {
            let mut accounts = self.accounts.lock().map_err(|_| "账号测试锁不可用")?;
            if !accounts.insert(request.account_id.clone()) {
                return json_response(409, &json!({"error":"该账号已有测试在执行"}));
            }
        }
        let _account = AccountGuard {
            accounts: self.accounts.clone(),
            id: request.account_id.clone(),
        };
        // 去重凭证先落盘再调用上游，响应丢失也不以同一ID重复消耗。
        {
            let _guard = self.history.lock().await;
            let (history, _) = store::get::<History>(host, "history").await?;
            if let Some(record) = history.records.iter().find(|r| r.id == request.id) {
                return json_response(200, &json!({"record":record,"replayed":true}));
            }
        }
        let account: AuthRuntimeAccount = payload_call(
            host,
            "host.auth.get_runtime",
            &AuthGetRequest {
                account_id: request.account_id.clone(),
            },
        )
        .await?;
        if !account.enabled {
            return Err("账号已停用".into());
        }
        let question = if request.mode == "question" {
            let (custom, _) = store::get::<CustomBank>(host, "bank").await?;
            questions::builtins()
                .into_iter()
                .chain(custom.questions)
                .find(|q| Some(&q.id) == request.question_id.as_ref())
                .ok_or("题目不存在，请刷新题库")?
                .into()
        } else {
            probe::eligibility(&account).map_err(str::to_owned)?;
            None
        };
        let started = now_ms();
        let record = Record {
            id: request.id.clone(),
            batch_id: request.batch_id.clone(),
            account_id: account.account_id.clone(),
            account_name: account.name.clone(),
            mode: request.mode.clone(),
            model: request.model.clone(),
            effort: request.effort.clone(),
            question_id: question.as_ref().map(|q: &Question| q.id.clone()),
            question_title: question.as_ref().map(|q| q.title.clone()),
            prompt: question.as_ref().map(|q| q.prompt.clone()),
            expected: question.as_ref().map(|q| q.answer.clone()),
            status: "running".into(),
            started_at_ms: started,
            finished_at_ms: None,
            latency_ms: None,
            answer: None,
            detail: "结果未确认，刷新后查看记录，不自动重试".into(),
            metrics: json!({}),
        };
        {
            let _guard = self.history.lock().await;
            let (mut history, version) = store::get::<History>(host, "history").await?;
            if let Some(record) = history.records.iter().find(|r| r.id == request.id) {
                return json_response(200, &json!({"record":record,"replayed":true}));
            }
            if history.records.iter().any(|r| {
                r.account_id == request.account_id
                    && r.status == "running"
                    && started.saturating_sub(r.started_at_ms) < 150_000
            }) {
                return json_response(409, &json!({"error":"该账号有未确认测试，请稍后查询历史"}));
            }
            history.append(record.clone())?;
            store::put(host, "history", &history, version).await?;
        }
        let mut pending = PendingAccount {
            guard: Some((_account, _slot)),
            completed: false,
        };
        let outcome = tokio::time::timeout(Duration::from_secs(90), async {
            if let Some(q) = question {
                execute_question(host, &request, &q).await
            } else {
                probe::run(host, &account, &request.model)
                    .await
                    .map(|r| (r.status, None, r.detail, r.metrics))
            }
        })
        .await
        .unwrap_or_else(|_| Err("测试超时，无法判断，未自动重试".into()));
        pending.completed = outcome.is_ok();
        let mut record = record;
        record.finished_at_ms = Some(now_ms());
        record.latency_ms = Some(now_ms().saturating_sub(started));
        match outcome {
            Ok((status, answer, detail, metrics)) => {
                record.status = status;
                record.answer = answer;
                record.detail = detail;
                record.metrics = metrics;
            }
            Err(error) => {
                record.status = if request.mode == "probe" {
                    "inconclusive"
                } else {
                    "failed"
                }
                .into();
                record.detail = error;
            }
        }
        let _guard = self.history.lock().await;
        let (mut history, version) = store::get::<History>(host, "history").await?;
        let existing = history
            .records
            .iter_mut()
            .find(|r| r.id == request.id)
            .ok_or("测试记录丢失，结果未保存")?;
        *existing = record.clone();
        store::put(host, "history", &history, version).await?;
        json_response(200, &json!({"record":record,"replayed":false}))
    }
}

type Outcome = (String, Option<String>, String, Value);
async fn execute_question(
    host: &HostClient,
    request: &RunRequest,
    question: &Question,
) -> Result<Outcome, String> {
    let mut body = json!({"model":request.model,"input":question.prompt,"store":false});
    if request.effort != "default" {
        body["reasoning"] = json!({"effort":request.effort});
    }
    let metadata = ModelExecuteRequest {
        client_key_id: request.client_key_id.clone(),
        model: request.model.clone(),
        protocol: "openai".into(),
        operation: ModelOperation::Generate,
        provider: None,
        account_id: Some(request.account_id.clone()),
        previous_response_id: None,
    };
    let reply = host
        .call(
            "host.model.execute",
            serde_json::to_value(metadata).map_err(|_| "模型请求编码失败")?,
            serde_json::to_vec(&body).map_err(|_| "题目正文编码失败")?,
        )
        .await
        .map_err(|_| "模型调用未完成，请核对Key范围、模型、账号与额度")?;
    let events = ModelEventBatch::decode(&reply.payload).map_err(|_| "模型事件格式无效")?;
    let (text, metrics) = collect_answer(&events.events)?;
    let status = if questions::grade(&text, &question.answer) {
        "correct"
    } else {
        "wrong"
    };
    Ok((
        status.into(),
        Some(text),
        if status == "correct" {
            "答案一致"
        } else {
            "答案与标准答案不一致"
        }
        .into(),
        metrics,
    ))
}

pub fn collect_answer(events: &[ExecutionEvent]) -> Result<(String, Value), String> {
    let mut text = String::new();
    let mut completed = false;
    let mut usage = Usage::default();
    let mut model = None;
    for event in events {
        for fact in &event.facts {
            match fact {
                CanonicalEvent::TextDelta { text: delta, .. } => {
                    if text.len() + delta.len() > 8192 {
                        return Err("答案超过8192字节，无法自动评分".into());
                    }
                    text.push_str(delta);
                }
                CanonicalEvent::Usage { usage: observed } => usage = observed.clone(),
                CanonicalEvent::Completed {
                    reason,
                    model: reported,
                    ..
                } => {
                    if !matches!(reason, FinishReason::Stop) {
                        return Err("模型未正常完成回答，无法评分".into());
                    }
                    completed = true;
                    model = reported.clone();
                }
                _ => {}
            }
        }
    }
    if !completed || text.trim().is_empty() {
        return Err("响应缺少完整答案，无法评分".into());
    }
    Ok((text, json!({"usage":usage,"reported_model":model})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completed_text_only() {
        let events = vec![
            ExecutionEvent::canonical(CanonicalEvent::ReasoningDelta {
                index: 0,
                text: "21".into(),
            }),
            ExecutionEvent::canonical(CanonicalEvent::TextDelta {
                index: 1,
                text: "20".into(),
            }),
            ExecutionEvent::canonical(CanonicalEvent::Completed {
                id: "resp".into(),
                model: None,
                reason: FinishReason::Stop,
            }),
        ];
        assert_eq!(collect_answer(&events).unwrap().0, "20");
        assert!(collect_answer(&events[..2]).is_err());
    }
    #[test]
    fn rejects_unknown_mode_and_effort() {
        let mut r = RunRequest {
            id: "abcdefgh".into(),
            batch_id: "abcdefgh".into(),
            account_id: "a".into(),
            mode: "probe".into(),
            model: "test".into(),
            effort: "default".into(),
            client_key_id: None,
            question_id: None,
        };
        assert!(r.validate().is_ok());
        r.effort = "unsafe".into();
        assert!(r.validate().is_err());
    }
}
