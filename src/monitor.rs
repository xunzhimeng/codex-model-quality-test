use crate::{
    accounts,
    app::{App, RunRequest},
    store::{self, Record, now_ms},
};
use gateway_plugin_sdk::client::HostClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tokio::sync::Mutex;

const MINUTE: u64 = 60_000;
const DAY: u64 = 1440 * MINUTE;
const OFFSET: u64 = 480 * MINUTE;
fn day(now: u64) -> u64 {
    now.saturating_add(OFFSET) / DAY
}
fn midnight_after(now: u64) -> u64 {
    (day(now) + 1) * DAY - OFFSET
}
fn default_daily_max() -> u32 {
    48
}
fn default_confirmation_delay() -> u32 {
    60
}
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradationPolicy {
    Immediate,
    #[default]
    Confirm,
}
impl DegradationPolicy {
    fn threshold(self) -> u8 {
        match self {
            Self::Immediate => 1,
            Self::Confirm => 2,
        }
    }
}
fn minute(text: &str) -> Option<u64> {
    if text.len() != 5 || text.as_bytes()[2] != b':' {
        return None;
    }
    let (h, m) = (
        text.get(..2)?.parse::<u64>().ok()?,
        text.get(3..)?.parse::<u64>().ok()?,
    );
    (h < 24
        && m < 60
        && text
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 2 || b.is_ascii_digit()))
    .then_some(h * 60 + m)
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuietHours {
    pub start: String,
    pub end: String,
}
impl QuietHours {
    fn validate(&self) -> Result<(), String> {
        match (minute(&self.start), minute(&self.end)) {
            (Some(start), Some(end)) if start != end => Ok(()),
            _ => Err("不测试时段须为有效HH:MM，开始与结束不能相同".into()),
        }
    }
    fn release(&self, now: u64) -> u64 {
        let (Some(start), Some(end)) = (minute(&self.start), minute(&self.end)) else {
            return now;
        };
        let local = now.saturating_add(OFFSET) % DAY;
        let current = local / MINUTE;
        let blocked = if start < end {
            current >= start && current < end
        } else {
            current >= start || current < end
        };
        if !blocked {
            return now;
        }
        let end_ms = end * MINUTE;
        now + if end_ms > local {
            end_ms - local
        } else {
            DAY - local + end_ms
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub scheduled: bool,
    pub auto_disable: bool,
    pub interval_minutes: u32,
    pub model: String,
    #[serde(default)]
    pub client_key_id: Option<String>,
    pub account_ids: Vec<String>,
    #[serde(default)]
    pub quiet_hours: Option<QuietHours>,
    #[serde(default = "default_daily_max")]
    pub daily_max: u32,
    #[serde(default)]
    pub degradation_policy: DegradationPolicy,
    #[serde(default = "default_confirmation_delay")]
    pub confirmation_delay_seconds: u32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            scheduled: false,
            auto_disable: false,
            interval_minutes: 30,
            model: String::new(),
            client_key_id: None,
            account_ids: vec![],
            quiet_hours: None,
            daily_max: default_daily_max(),
            degradation_policy: DegradationPolicy::default(),
            confirmation_delay_seconds: default_confirmation_delay(),
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        if !(5..=1440).contains(&self.interval_minutes) {
            return Err("间隔须为5至1440分钟".into());
        }
        if !(1..=1440).contains(&self.daily_max) {
            return Err("每日每账号上限须为1至1440次".into());
        }
        if !(1..=86400).contains(&self.confirmation_delay_seconds) {
            return Err("复测等待须为1至86400秒".into());
        }
        if let Some(quiet) = &self.quiet_hours {
            quiet.validate()?;
        }
        if self.auto_disable && !self.scheduled {
            return Err("自动停用须同时启用后台定时探针".into());
        }
        if (self.scheduled && self.model.is_empty())
            || self.model.trim() != self.model
            || self.model.len() > 128
            || self.model.bytes().any(|b| b.is_ascii_control())
        {
            return Err("探针模型无效".into());
        }
        if self.scheduled
            && self
                .client_key_id
                .as_ref()
                .is_none_or(|id| id.trim().is_empty() || id.len() > 128)
        {
            return Err("启用监控须选择客户端Key".into());
        }
        if self.account_ids.len() > 50 || (self.scheduled && self.account_ids.is_empty()) {
            return Err("启用监控须选择1至50个账号".into());
        }
        let ids: BTreeSet<_> = self.account_ids.iter().collect();
        if ids.len() != self.account_ids.len()
            || ids.iter().any(|id| id.is_empty() || id.len() > 128)
        {
            return Err("监控账号无效或重复".into());
        }
        Ok(())
    }
    fn confirmation_due(&self, now: u64, account: &AccountState) -> Option<u64> {
        let due = now + u64::from(self.confirmation_delay_seconds) * 1000;
        // 配置的等待可长于常规间隔，但不得跨过禁测时段后继续累积降级证据。
        (self.release(due, account) == due && self.confirmation_current(now, due + 1, due))
            .then_some(due)
    }
    fn confirmation_current(&self, due: u64, regular_due: u64, now: u64) -> bool {
        if now >= regular_due {
            return false;
        }
        let Some(quiet) = &self.quiet_hours else {
            return true;
        };
        if quiet.release(due) > due || quiet.release(now) > now {
            return false;
        }
        let Some(start) = minute(&quiet.start) else {
            return false;
        };
        let local = due.saturating_add(OFFSET) % DAY;
        let until_start = (start * MINUTE + DAY - local) % DAY;
        now.saturating_sub(due) < until_start
    }
    fn release(&self, now: u64, account: &AccountState) -> u64 {
        let now = if account.today(now) >= u64::from(self.daily_max) {
            midnight_after(now)
        } else {
            now
        };
        self.quiet_hours.as_ref().map_or(now, |q| q.release(now))
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ActiveRun {
    id: String,
    started_ms: u64,
    confirmation: bool,
    counted: bool,
    valid: bool,
}
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountState {
    pub next_due_ms: u64,
    pub streak: u8,
    pub last_record_id: String,
    pub last_status: String,
    pub last_checked_ms: Option<u64>,
    pub action: String,
    pub action_detail: String,
    pub paused: bool,
    pub total_runs: u64,
    pub daily_runs: u64,
    pub count_day: u64,
    pub confirm_due_ms: Option<u64>,
    pub active: Option<ActiveRun>,
}
impl AccountState {
    fn today(&self, now: u64) -> u64 {
        if self.count_day == day(now) {
            self.daily_runs
        } else {
            0
        }
    }
    fn reset_confirmation(&mut self) {
        self.streak = 0;
        self.confirm_due_ms = None;
        if let Some(active) = &mut self.active {
            active.valid = false;
        }
    }
    fn count(&mut self, now: u64) {
        if self.count_day != day(now) {
            self.count_day = day(now);
            self.daily_runs = 0;
        }
        self.total_runs = self.total_runs.saturating_add(1);
        self.daily_runs = self.daily_runs.saturating_add(1);
    }
}
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub probe_version: u32,
    pub settings: Settings,
    pub generation: String,
    pub accounts: BTreeMap<String, AccountState>,
    pub last_tick_ms: Option<u64>,
    pub last_error: Option<String>,
    pub paused: bool,
    pub control_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Save {
    pub task_id: Option<String>,
    pub name: String,
    pub settings: Settings,
    pub expected_generation: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    pub task_id: Option<String>,
    pub account_id: Option<String>,
    pub paused: bool,
    pub expected_generation: String,
    pub expected_control_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delete {
    pub task_id: String,
    pub expected_generation: String,
    pub expected_control_revision: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub state: State,
}
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Registry {
    pub tasks_version: u32,
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub control_revision: u64,
}
fn bounded_error(error: String) -> String {
    // 宿主回调错误不应使整个任务集合无法写回，留存短安全摘要。
    error
        .chars()
        .filter(|ch| !ch.is_control())
        .take(120)
        .collect()
}
impl Registry {
    fn validate_capacity(&self) -> Result<(), String> {
        let accounts: usize = self
            .tasks
            .iter()
            .map(|task| {
                task.state
                    .accounts
                    .iter()
                    .filter(|(id, status)| {
                        task.state.settings.account_ids.contains(id) || status.active.is_some()
                    })
                    .count()
            })
            .sum();
        // 为所有保留账号的计数、预约、结果和有界停用审计预留余量，不只预算当前空配置。
        let reserve = accounts.saturating_mul(1024) + self.tasks.len() * 1024 + 1024;
        if store::state_metadata_bytes("monitor", self)?.saturating_add(reserve)
            > store::STATE_FRAME_BUDGET
        {
            return Err("监控任务总容量不足，请减少新任务账号或配置，原计数和审计不会删除".into());
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct RunContext {
    pub task_id: String,
    pub generation: String,
}
#[derive(Default)]
pub struct Monitor {
    lock: Mutex<()>,
}
impl Monitor {
    async fn load(&self, host: &HostClient) -> Result<(Registry, Option<u64>), String> {
        let (value, version) = store::get::<Value>(host, "monitor").await?;
        if value.is_null() {
            return Ok((
                Registry {
                    tasks_version: 1,
                    ..Registry::default()
                },
                version,
            ));
        }
        if value.get("tasks_version").is_some() {
            let registry: Registry =
                serde_json::from_value(value).map_err(|_| "监控任务状态无法读取")?;
            if registry.tasks_version != 1 {
                return Err("监控任务状态版本不支持".into());
            }
            return Ok((registry, version));
        }
        let mut state: State = serde_json::from_value(value).map_err(|_| "旧监控状态无法读取")?;
        // 用户确认升级不自动发请求，保留设置、计数和审计，只暂停与作废旧确认。
        state.paused = true;
        state.generation = uuid::Uuid::new_v4().to_string();
        for status in state.accounts.values_mut() {
            status.reset_confirmation();
        }
        state.probe_version = 2;
        state.last_error = Some("旧计划已迁为暂停任务，请核对配置后继续".into());
        let registry = Registry {
            tasks_version: 1,
            tasks: vec![Task {
                id: uuid::Uuid::new_v4().to_string(),
                name: "原监控计划".into(),
                state,
            }],
            control_revision: 0,
        };
        store::put(host, "monitor", &registry, version).await?;
        store::get(host, "monitor").await
    }
    fn task_snapshot(task: &Task, now: u64) -> Value {
        let mut state = task.state.clone();
        let mut total = 0_u64;
        let mut today = 0_u64;
        let mut availability = BTreeMap::new();
        for (id, status) in &mut state.accounts {
            status.daily_runs = status.today(now);
            status.count_day = day(now);
            total = total.saturating_add(status.total_runs);
            today = today.saturating_add(status.daily_runs);
            if !state.settings.account_ids.contains(id) {
                continue;
            }
            let reason = if !state.settings.scheduled {
                "未启用"
            } else if state.paused || status.paused {
                "已暂停"
            } else if status.action == "disabled" {
                "已自动停用，需人工恢复"
            } else if status.active.is_some() {
                "执行中或结果未确认"
            } else if status.daily_runs >= u64::from(state.settings.daily_max) {
                "今日已达上限"
            } else if state
                .settings
                .quiet_hours
                .as_ref()
                .is_some_and(|q| q.release(now) > now)
            {
                "不测试时段"
            } else if status.confirm_due_ms.is_some() {
                "等待确认复测"
            } else {
                "等待常规测试"
            };
            let due = status.confirm_due_ms.unwrap_or(status.next_due_ms).max(now);
            let next = (state.settings.scheduled
                && !state.paused
                && !status.paused
                && status.active.is_none()
                && status.action != "disabled")
                .then(|| state.settings.release(due, status));
            availability.insert(id.clone(), json!({"status":reason,"next_allowed_ms":next}));
        }
        json!({"id":task.id,"name":task.name,"state":state,"availability":availability,"total_runs":total,"daily_runs":today})
    }
    pub async fn snapshot(&self, host: &HostClient) -> Result<Value, String> {
        let _guard = self.lock.lock().await;
        let (registry, version) = self.load(host).await?;
        let now = now_ms();
        let (history, _) = store::get::<crate::store::History>(host, "history").await?;
        let tasks: Vec<_> = registry
            .tasks
            .iter()
            .map(|task| {
                let mut snapshot = Self::task_snapshot(task, now);
                // 结果详情由历史唯一持有，监控只按凭证投影，避免额外持久化和容量膨胀。
                for (id, status) in &task.state.accounts {
                    if let Some(record) = history.records.iter().find(|r| {
                        r.id == status.last_record_id
                            && &r.account_id == id
                            && r.status == status.last_status
                    }) {
                        snapshot["state"]["accounts"][id]["last_result"] = json!(record);
                    }
                }
                snapshot
            })
            .collect();
        let total: u64 = tasks.iter().filter_map(|t| t["total_runs"].as_u64()).sum();
        let today: u64 = tasks.iter().filter_map(|t| t["daily_runs"].as_u64()).sum();
        Ok(
            json!({"tasks":tasks,"version":version,"control_revision":registry.control_revision,"defaults":Settings::default(),"total_runs":total,"daily_runs":today,"timezone":"Asia/Shanghai","now_ms":now}),
        )
    }
    pub async fn save(&self, host: &HostClient, request: Save) -> Result<Value, String> {
        request.settings.validate()?;
        if request.settings.scheduled {
            for id in &request.settings.account_ids {
                let account: gateway_plugin_sdk::call::host::AuthRuntimeAccount =
                    store::payload_call(
                        host,
                        "host.auth.get_runtime",
                        &gateway_plugin_sdk::call::host::AuthGetRequest {
                            account_id: id.clone(),
                        },
                    )
                    .await?;
                let info = accounts::get(host, id).await?;
                crate::key_scope::check(
                    host,
                    request
                        .settings
                        .client_key_id
                        .as_deref()
                        .unwrap_or_default(),
                    &request.settings.model,
                    &info,
                )
                .await?;
                if account.provider_id != "openai" || account.authentication_kind != "oauth" {
                    return Err("监控仅支持OpenAI OAuth账号".into());
                }
            }
        }
        if request.name.trim().is_empty()
            || request.name.len() > 120
            || request.name.chars().any(char::is_control)
        {
            return Err("任务名称须为1至120字节且不含控制字符".into());
        }
        let _guard = self.lock.lock().await;
        let (mut registry, version) = self.load(host).await?;
        let previous_bytes = store::state_metadata_bytes("monitor", &registry)?;
        let index = if let Some(id) = &request.task_id {
            registry
                .tasks
                .iter()
                .position(|t| &t.id == id)
                .ok_or("监控任务不存在，请刷新")?
        } else {
            if request.expected_generation.is_some() {
                return Err("创建任务不能携带旧设置版本".into());
            }
            if registry.tasks.len() >= 10 {
                return Err("最多创建10个监控任务".into());
            }
            registry.tasks.push(Task {
                id: uuid::Uuid::new_v4().to_string(),
                name: String::new(),
                state: State::default(),
            });
            registry.tasks.len() - 1
        };
        let task = &mut registry.tasks[index];
        let state = &mut task.state;
        if (!state.generation.is_empty()).then_some(state.generation.as_str())
            != request.expected_generation.as_deref()
        {
            return Err("监控设置已变化，请刷新后再保存".into());
        }
        task.name = request.name.trim().into();
        state.probe_version = 2;
        state.settings = request.settings;
        state.generation = uuid::Uuid::new_v4().to_string();
        state.last_error = None;
        state.control_revision = state.control_revision.saturating_add(1);
        // 计数和停用审计与设置代次分离，保存、移除后重新选择都不能清掉当日额度。
        for status in state.accounts.values_mut() {
            status.reset_confirmation();
        }
        for id in &state.settings.account_ids {
            state.accounts.entry(id.clone()).or_default().next_due_ms =
                now_ms() + u64::from(state.settings.interval_minutes) * MINUTE;
        }
        registry.control_revision = registry.control_revision.saturating_add(1);
        if let Err(error) = registry.validate_capacity()
            && !(request.task_id.is_some()
                && !registry.tasks[index].state.settings.scheduled
                && store::state_metadata_bytes("monitor", &registry)? <= previous_bytes + 128)
        {
            return Err(error);
        }
        store::put(host, "monitor", &registry, version).await?;
        drop(_guard);
        self.snapshot(host).await
    }
    pub async fn delete(&self, host: &HostClient, request: Delete) -> Result<Value, String> {
        let _guard = self.lock.lock().await;
        let (mut registry, version) = self.load(host).await?;
        let index = registry
            .tasks
            .iter()
            .position(|task| task.id == request.task_id)
            .ok_or("监控任务不存在，请刷新列表")?;
        let state = &registry.tasks[index].state;
        if state.generation != request.expected_generation
            || state.control_revision != request.expected_control_revision
        {
            return Err("监控任务已变化，请刷新后再删除".into());
        }
        let now = now_ms();
        // 不丢弃在途凭证或未确认停用意图，否则其他任务可能重叠请求或重复提交停用。
        if state.accounts.values().any(|status| {
            status.active.as_ref().is_some_and(|run| {
                now.saturating_sub(run.started_ms) < store::PENDING_PROTECTION_MS
            })
        }) {
            return Err("任务仍有执行中或隔离中的测试，请先暂停，等待结束后再删除".into());
        }
        if state
            .accounts
            .values()
            .any(|status| matches!(status.action.as_str(), "pending" | "unconfirmed"))
        {
            return Err("任务存在未确认停用动作，暂不可删除，以免丢失安全审计或重复停用".into());
        }
        registry.tasks.remove(index);
        registry.control_revision = registry.control_revision.saturating_add(1);
        store::put(host, "monitor", &registry, version).await?;
        drop(_guard);
        self.snapshot(host).await
    }
    pub async fn control(&self, host: &HostClient, request: Control) -> Result<Value, String> {
        let _guard = self.lock.lock().await;
        let (mut registry, version) = self.load(host).await?;
        if let Some(id) = &request.task_id {
            let task = registry
                .tasks
                .iter()
                .find(|t| &t.id == id)
                .ok_or("监控任务不存在")?;
            if task.state.generation != request.expected_generation
                || task.state.control_revision != request.expected_control_revision
            {
                return Err("监控控制状态已变化，请刷新后再操作".into());
            }
        } else if request.account_id.is_some()
            || registry.control_revision != request.expected_control_revision
        {
            return Err("监控控制状态已变化，请刷新后再操作".into());
        }
        for task in &mut registry.tasks {
            if request.task_id.as_ref().is_some_and(|id| id != &task.id) {
                continue;
            }
            let state = &mut task.state;
            if !request.paused && state.settings.scheduled {
                state.settings.validate()?;
            }
            let ids = if let Some(id) = &request.account_id {
                if !state.settings.account_ids.contains(id) {
                    return Err("账号不在当前任务中".into());
                }
                state.accounts.get_mut(id).ok_or("监控账号状态缺失")?.paused = request.paused;
                vec![id.clone()]
            } else {
                state.paused = request.paused;
                state.settings.account_ids.clone()
            };
            for id in ids {
                if let Some(status) = state.accounts.get_mut(&id) {
                    status.reset_confirmation();
                    if !request.paused {
                        status.next_due_ms =
                            now_ms() + u64::from(state.settings.interval_minutes) * MINUTE;
                    }
                }
            }
            state.control_revision = state.control_revision.saturating_add(1);
        }
        registry.control_revision = registry.control_revision.saturating_add(1);
        if !request.paused {
            registry.validate_capacity()?;
        }
        store::put(host, "monitor", &registry, version).await?;
        drop(_guard);
        self.snapshot(host).await
    }
    pub async fn begin(
        &self,
        host: &HostClient,
        request: &RunRequest,
        context: &RunContext,
    ) -> Result<bool, String> {
        let _guard = self.lock.lock().await;
        let (mut registry, version) = self.load(host).await?;
        let Some(task) = registry.tasks.iter_mut().find(|t| t.id == context.task_id) else {
            return Ok(false);
        };
        let state = &mut task.state;
        let now = now_ms();
        if state.generation != context.generation || !state.settings.scheduled || state.paused {
            return Ok(false);
        }
        let Some(status) = state.accounts.get_mut(&request.account_id) else {
            return Ok(false);
        };
        if status.paused {
            return Ok(false);
        }
        if state.settings.release(now, status) > now {
            return Ok(false);
        }
        let Some(active) = &status.active else {
            return Ok(false);
        };
        if active.id != request.id || !active.valid || active.counted {
            return Ok(false);
        }
        status.count(now);
        status.active.as_mut().unwrap().counted = true;
        status.last_status = "running".into();
        store::put(host, "monitor", &registry, version).await?;
        Ok(true)
    }
    pub async fn observe(
        &self,
        host: &HostClient,
        record: &mut Record,
        context: &RunContext,
    ) -> Result<(), String> {
        let _guard = self.lock.lock().await;
        let (mut registry, version) = self.load(host).await?;
        let unresolved_action = registry.tasks.iter().any(|task| {
            task.state
                .accounts
                .get(&record.account_id)
                .is_some_and(|status| matches!(status.action.as_str(), "pending" | "unconfirmed"))
        });
        let Some(task) = registry.tasks.iter_mut().find(|t| t.id == context.task_id) else {
            return Ok(());
        };
        let task_name = task.name.clone();
        let state = &mut task.state;
        let now = record.finished_at_ms.unwrap_or_else(now_ms);
        let Some(status) = state.accounts.get_mut(&record.account_id) else {
            return Ok(());
        };
        let Some(active) = &status.active else {
            return Ok(());
        };
        if active.id != record.id || !active.counted {
            return Ok(());
        }
        let active = status.active.take().unwrap();
        status.last_record_id = record.id.clone();
        status.last_checked_ms = Some(now);
        status.last_status = record.status.clone();
        let valid = active.valid
            && state.generation == context.generation
            && state.settings.scheduled
            && !state.paused
            && !status.paused
            && state.settings.model == record.model
            && state.settings.client_key_id.as_deref()
                == record.metrics.get("client_key_id").and_then(Value::as_str);
        status.streak = 0;
        status.confirm_due_ms = None;
        status.next_due_ms = now + u64::from(state.settings.interval_minutes) * MINUTE;
        let policy = state.settings.degradation_policy;
        let threshold = policy.threshold();
        let mut should_disable = false;
        if valid {
            if status.action == "disabled" {
                status.action.clear();
                status.action_detail.clear();
            }
            if record.status == "degraded" {
                if policy == DegradationPolicy::Immediate || active.confirmation {
                    status.streak = threshold;
                    should_disable = state.settings.auto_disable
                        && status.action.is_empty()
                        && !unresolved_action;
                } else if let Some(due) = state.settings.confirmation_due(now, status) {
                    status.streak = 1;
                    status.confirm_due_ms = Some(due);
                    // 先完成预约确认，避免等待秒数长于常规间隔时被新一轮覆盖。
                    status.next_due_ms = due + u64::from(state.settings.interval_minutes) * MINUTE;
                }
            }
        }
        if should_disable {
            status.action = "pending".into();
            status.action_detail = if policy == DegradationPolicy::Immediate {
                "首次疑似降级，立即停用待确认"
            } else {
                "复测仍疑似降级，停用待确认"
            }
            .into();
        }
        record.metrics["monitor"] = json!({"task_id":context.task_id,"task_name":task_name,"streak":status.streak,"threshold":threshold,"confirmation":active.confirmation,"policy":policy,"action":status.action,"detail":status.action_detail});
        if unresolved_action && status.action.is_empty() {
            record.metrics["monitor"]["detail"] =
                json!("同账号已有未确认停用动作，未重复提交，请核对原任务审计");
        }
        store::put(host, "monitor", &registry, version).await?;
        if !should_disable {
            return Ok(());
        }
        let result = accounts::disable(host, &record.account_id).await;
        let (mut registry, version) = self.load(host).await?;
        let Some(task) = registry.tasks.iter_mut().find(|t| t.id == context.task_id) else {
            return Ok(());
        };
        let task_name = task.name.clone();
        let state = &mut task.state;
        if let Some(status) = state.accounts.get_mut(&record.account_id) {
            match result {
                Ok(()) => {
                    status.action = "disabled".into();
                    status.action_detail = if policy == DegradationPolicy::Immediate {
                        "首次疑似降级，账号已停用，需手动恢复"
                    } else {
                        "确认复测仍疑似降级，账号已停用，需手动恢复"
                    }
                    .into();
                }
                Err(error) => {
                    status.action = "unconfirmed".into();
                    status.action_detail = bounded_error(error);
                }
            }
            record.metrics["monitor"] = json!({"task_id":context.task_id,"task_name":task_name,"streak":status.streak,"threshold":threshold,"confirmation":active.confirmation,"policy":policy,"action":status.action,"detail":status.action_detail});
        }
        store::put(host, "monitor", &registry, version).await
    }
    pub async fn tick(&self, app: &App, host: &HostClient) -> Result<(), String> {
        let job = {
            let _guard = self.lock.lock().await;
            let (mut registry, version) = self.load(host).await?;
            let now = now_ms();
            for task in &mut registry.tasks {
                let state = &mut task.state;
                state.last_tick_ms = Some(now);
                for status in state.accounts.values_mut() {
                    if status.active.as_ref().is_some_and(|run| {
                        now.saturating_sub(run.started_ms) >= store::PENDING_PROTECTION_MS
                    }) {
                        status.active = None;
                        status.reset_confirmation();
                        status.last_status = "interrupted".into();
                    }
                    if status.confirm_due_ms.is_some_and(|due| {
                        due <= now
                            && (state.settings.release(now, status) > now
                                || !state.settings.confirmation_current(
                                    due,
                                    status.next_due_ms,
                                    now,
                                ))
                    }) {
                        status.reset_confirmation();
                    }
                }
            }
            // 跨任务共享账号互斥；未结束预约在重启后同样阻止其他任务重叠发送。
            let active: BTreeSet<_> = registry
                .tasks
                .iter()
                .flat_map(|t| t.state.accounts.iter())
                .filter(|(_, s)| s.active.is_some())
                .map(|(id, _)| id.clone())
                .collect();
            let mut due = Vec::new();
            for (index, task) in registry.tasks.iter().enumerate() {
                let state = &task.state;
                if !state.settings.scheduled || state.paused {
                    continue;
                }
                for id in &state.settings.account_ids {
                    let Some(status) = state.accounts.get(id) else {
                        continue;
                    };
                    let at = status.confirm_due_ms.unwrap_or(status.next_due_ms);
                    if !status.paused
                        && !active.contains(id)
                        && !app.account_busy(id)
                        && at <= now
                        && state.settings.release(now, status) == now
                    {
                        due.push((at, index, id.clone()));
                    }
                }
            }
            due.sort();
            let Some((_, index, id)) = due.into_iter().next() else {
                store::put(host, "monitor", &registry, version).await?;
                return Ok(());
            };
            let task = &mut registry.tasks[index];
            let state = &mut task.state;
            let status = state.accounts.get_mut(&id).unwrap();
            let request_id = uuid::Uuid::new_v4().to_string();
            let confirmation = status.confirm_due_ms.take().is_some();
            status.active = Some(ActiveRun {
                id: request_id.clone(),
                started_ms: now,
                confirmation,
                counted: false,
                valid: true,
            });
            status.next_due_ms = now + u64::from(state.settings.interval_minutes) * MINUTE;
            state.last_error = None;
            let context = RunContext {
                task_id: task.id.clone(),
                generation: state.generation.clone(),
            };
            let job = (
                id,
                request_id,
                state.settings.model.clone(),
                context,
                state.settings.client_key_id.clone(),
            );
            store::put(host, "monitor", &registry, version).await?;
            job
        };
        let request = RunRequest {
            id: job.1.clone(),
            batch_id: uuid::Uuid::new_v4().to_string(),
            account_id: job.0.clone(),
            mode: "probe".into(),
            model: job.2,
            effort: "default".into(),
            client_key_id: job.4,
            question_id: None,
        };
        let result = app.run(host, request, 180, Some(job.3.clone()), true).await;
        if let Err(error) = result {
            let _guard = self.lock.lock().await;
            let (mut registry, version) = self.load(host).await?;
            if let Some(task) = registry.tasks.iter_mut().find(|t| t.id == job.3.task_id) {
                let state = &mut task.state;
                if let Some(status) = state.accounts.get_mut(&job.0)
                    && status.active.as_ref().is_some_and(|r| r.id == job.1)
                {
                    status.active = None;
                    status.reset_confirmation();
                    status.last_status = "skipped".into();
                    status.last_checked_ms = Some(now_ms());
                    state.last_error = Some(bounded_error(error));
                    store::put(host, "monitor", &registry, version).await?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_outlives_declared_maintenance_call() {
        let budgets =
            gateway_plugin_sdk::ResolvedCallBudgets::resolve(&crate::call_budgets()).unwrap();
        assert!(
            store::PENDING_PROTECTION_MS as u128
                > budgets
                    .budget(gateway_plugin_sdk::Stage::Maintenance)
                    .as_millis()
        );
    }
    #[test]
    fn quiet_hours_cross_midnight_and_beijing_day_boundary() {
        let now = |h: u64, m: u64| (10 * DAY + h * 60 * MINUTE + m * MINUTE) - OFFSET;
        let q = QuietHours {
            start: "23:00".into(),
            end: "07:00".into(),
        };
        assert_eq!(q.release(now(23, 0)), now(24 + 7, 0));
        assert_eq!(q.release(now(6, 59)), now(7, 0));
        assert_eq!(q.release(now(7, 0)), now(7, 0));
        assert_eq!(day(now(0, 0) - 1), 9);
        assert_eq!(day(now(0, 0)), 10);
        assert_eq!(midnight_after(now(23, 59)), now(24, 0));
        assert!(
            QuietHours {
                start: "07:00".into(),
                end: "07:00".into()
            }
            .validate()
            .is_err()
        );
        assert!(minute("7:00").is_none());
    }
    #[test]
    fn confirmation_expires_across_quiet_hours_or_regular_period() {
        let beijing = |h: u64, m: u64| 10 * DAY + (h * 60 + m) * MINUTE - OFFSET;
        let mut settings = Settings::default();
        assert!(settings.confirmation_current(beijing(8, 1), beijing(8, 30), beijing(8, 2)));
        assert!(!settings.confirmation_current(beijing(8, 1), beijing(8, 30), beijing(8, 30)));
        settings.quiet_hours = Some(QuietHours {
            start: "09:00".into(),
            end: "10:00".into(),
        });
        assert!(settings.confirmation_current(beijing(8, 59), beijing(12, 0), beijing(8, 59)));
        assert!(!settings.confirmation_current(beijing(8, 59), beijing(12, 0), beijing(10, 0)));
    }
    #[test]
    fn configurable_confirmation_respects_time_and_quota() {
        let beijing = |h: u64, m: u64| 10 * DAY + (h * 60 + m) * MINUTE - OFFSET;
        let mut settings = Settings {
            confirmation_delay_seconds: 3600,
            ..Settings::default()
        };
        let account = AccountState::default();
        assert_eq!(
            settings.confirmation_due(beijing(8, 0), &account),
            Some(beijing(9, 0))
        );
        settings.quiet_hours = Some(QuietHours {
            start: "09:00".into(),
            end: "10:00".into(),
        });
        assert_eq!(settings.confirmation_due(beijing(8, 0), &account), None);
        settings.confirmation_delay_seconds = 3 * 3600;
        assert_eq!(settings.confirmation_due(beijing(8, 0), &account), None);
        settings.quiet_hours = None;
        settings.confirmation_delay_seconds = 1;
        let mut account = AccountState::default();
        account.count(beijing(8, 0));
        settings.daily_max = 1;
        assert_eq!(settings.confirmation_due(beijing(8, 0), &account), None);
        settings.confirmation_delay_seconds = 86400;
        assert_eq!(
            settings.confirmation_due(beijing(8, 0), &account),
            Some(beijing(32, 0))
        );
        for delay in [0, 86401] {
            settings.confirmation_delay_seconds = delay;
            assert!(settings.validate().is_err());
        }
        assert_eq!(DegradationPolicy::Immediate.threshold(), 1);
        assert_eq!(DegradationPolicy::Confirm.threshold(), 2);
    }
    #[test]
    fn registry_reserves_capacity_before_saving_configurations() {
        let mut registry = Registry {
            tasks_version: 1,
            ..Registry::default()
        };
        let mut rejected = false;
        for i in 0..10 {
            let mut state = State::default();
            for j in 0..10 {
                let id = format!("account-{i}-{j}");
                state.settings.account_ids.push(id.clone());
                state.accounts.insert(id, AccountState::default());
            }
            registry.tasks.push(Task {
                id: i.to_string(),
                name: "任务".into(),
                state,
            });
            if registry.validate_capacity().is_err() {
                rejected = true;
                break;
            }
            // 所有获准账号都增长为执行、满位数计数和有界审计，仍能写入。
            let mut grown = registry.clone();
            for task in &mut grown.tasks {
                task.state.last_error = Some("错".repeat(120));
                for status in task.state.accounts.values_mut() {
                    status.total_runs = u64::MAX;
                    status.daily_runs = u64::MAX;
                    status.count_day = u64::MAX;
                    status.last_record_id = "x".repeat(64);
                    status.last_status = "inconclusive".into();
                    status.last_checked_ms = Some(u64::MAX);
                    status.confirm_due_ms = Some(u64::MAX);
                    status.action = "unconfirmed".into();
                    status.action_detail = "错".repeat(120);
                    status.active = Some(ActiveRun {
                        id: "x".repeat(64),
                        started_ms: u64::MAX,
                        counted: true,
                        confirmation: true,
                        valid: true,
                    });
                }
            }
            assert!(
                store::state_metadata_bytes("monitor", &grown).unwrap()
                    <= store::STATE_FRAME_BUDGET
            );
        }
        assert!(rejected);
    }
    #[test]
    fn settings_opt_in_and_quota_survives_days() {
        let mut settings = Settings::default();
        assert!(settings.validate().is_ok());
        settings.auto_disable = true;
        assert!(settings.validate().is_err());
        settings.scheduled = true;
        assert!(settings.validate().is_err());
        settings.model = "model".into();
        settings.client_key_id = Some("key".into());
        settings.account_ids = vec!["account".into()];
        assert!(settings.validate().is_ok());
        settings.daily_max = 0;
        assert!(settings.validate().is_err());
        let now = 10 * DAY;
        let mut status = AccountState::default();
        status.count(now);
        assert_eq!(status.today(now), 1);
        assert_eq!(status.today(now + DAY), 0);
        assert_eq!(status.total_runs, 1);
        settings.daily_max = 1;
        assert_eq!(settings.release(now, &status), midnight_after(now));
        // 每个账号单独计数，前一个账号的额度不阻止另一个账号。
        assert_eq!(settings.release(now, &AccountState::default()), now);
        settings.quiet_hours = Some(QuietHours {
            start: "00:00".into(),
            end: "07:00".into(),
        });
        assert_eq!(
            settings.release(now, &status),
            midnight_after(now) + 7 * 60 * MINUTE
        );
        let window = QuietHours {
            start: "09:00".into(),
            end: "10:00".into(),
        };
        let beijing = |hour: u64| 10 * DAY + hour * 60 * MINUTE - OFFSET;
        assert_eq!(window.release(beijing(9)), beijing(10));
        assert_eq!(window.release(beijing(10)), beijing(10));
        status.count(now + DAY);
        assert_eq!(status.daily_runs, 1);
        assert_eq!(status.total_runs, 2);
    }
}
