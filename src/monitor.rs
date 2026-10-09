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
const RUN_LEASE: u64 = 150_000;
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
    pub settings: Settings,
    pub expected_generation: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    pub account_id: Option<String>,
    pub paused: bool,
    pub expected_generation: String,
    pub expected_control_revision: u64,
}
#[derive(Default)]
pub struct Monitor {
    lock: Mutex<()>,
}
impl Monitor {
    async fn load(&self, host: &HostClient) -> Result<(State, Option<u64>), String> {
        let (mut state, version) = store::get::<State>(host, "monitor").await?;
        if version.is_some() && state.probe_version < 2 {
            // 新确认策略不继承旧命中，也不悄悄替现有计划增加自动请求。
            state.probe_version = 2;
            state.settings.scheduled = false;
            state.settings.auto_disable = false;
            state.generation = uuid::Uuid::new_v4().to_string();
            for status in state.accounts.values_mut() {
                status.reset_confirmation();
                status.active = None;
            }
            state.last_error = Some(
                "监控规则已更新，旧计划已关闭，请核对每日上限与不测试时段后重新保存启用".into(),
            );
            store::put(host, "monitor", &state, version).await?;
            return store::get::<State>(host, "monitor").await;
        }
        Ok((state, version))
    }
    pub async fn snapshot(&self, host: &HostClient) -> Result<Value, String> {
        let _guard = self.lock.lock().await;
        let (mut state, version) = self.load(host).await?;
        let now = now_ms();
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
        Ok(
            json!({"state":state,"version":version,"availability":availability,"total_runs":total,"daily_runs":today,"timezone":"Asia/Shanghai","now_ms":now}),
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
                if info.proxy_endpoint.as_ref().is_none_or(|s| s.is_empty()) {
                    return Err("监控账号未配置代理".into());
                }
                if account.provider_id != "openai" || account.authentication_kind != "oauth" {
                    return Err("监控仅支持OpenAI OAuth账号".into());
                }
            }
        }
        let _guard = self.lock.lock().await;
        let (mut state, version) = self.load(host).await?;
        if (!state.generation.is_empty()).then_some(state.generation.as_str())
            != request.expected_generation.as_deref()
        {
            return Err("监控设置已变化，请刷新后再保存".into());
        }
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
        store::put(host, "monitor", &state, version).await?;
        drop(_guard);
        self.snapshot(host).await
    }
    pub async fn control(&self, host: &HostClient, request: Control) -> Result<Value, String> {
        let _guard = self.lock.lock().await;
        let (mut state, version) = self.load(host).await?;
        if state.generation != request.expected_generation
            || state.control_revision != request.expected_control_revision
        {
            return Err("监控控制状态已变化，请刷新后再操作".into());
        }
        let ids = if let Some(id) = request.account_id {
            if !state.settings.account_ids.contains(&id) {
                return Err("账号不在当前监控计划中".into());
            }
            state
                .accounts
                .get_mut(&id)
                .ok_or("监控账号状态缺失")?
                .paused = request.paused;
            vec![id]
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
        store::put(host, "monitor", &state, version).await?;
        drop(_guard);
        self.snapshot(host).await
    }
    pub async fn begin(
        &self,
        host: &HostClient,
        request: &RunRequest,
        generation: &str,
    ) -> Result<bool, String> {
        let _guard = self.lock.lock().await;
        let (mut state, version) = self.load(host).await?;
        let now = now_ms();
        if state.generation != generation || !state.settings.scheduled || state.paused {
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
        store::put(host, "monitor", &state, version).await?;
        Ok(true)
    }
    pub async fn observe(
        &self,
        host: &HostClient,
        record: &mut Record,
        generation: &str,
    ) -> Result<(), String> {
        let _guard = self.lock.lock().await;
        let (mut state, version) = self.load(host).await?;
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
            && state.generation == generation
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
                    should_disable = state.settings.auto_disable && status.action.is_empty();
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
        record.metrics["monitor"] = json!({"streak":status.streak,"threshold":threshold,"confirmation":active.confirmation,"policy":policy,"action":status.action,"detail":status.action_detail});
        store::put(host, "monitor", &state, version).await?;
        if !should_disable {
            return Ok(());
        }
        let result = accounts::disable(host, &record.account_id).await;
        let (mut state, version) = self.load(host).await?;
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
                    status.action_detail = error;
                }
            }
            record.metrics["monitor"] = json!({"streak":status.streak,"threshold":threshold,"confirmation":active.confirmation,"policy":policy,"action":status.action,"detail":status.action_detail});
        }
        store::put(host, "monitor", &state, version).await
    }
    pub async fn tick(&self, app: &App, host: &HostClient) -> Result<(), String> {
        let job = {
            let _guard = self.lock.lock().await;
            let (mut state, version) = self.load(host).await?;
            if !state.settings.scheduled {
                return Ok(());
            }
            let now = now_ms();
            state.last_tick_ms = Some(now);
            for status in state.accounts.values_mut() {
                if status
                    .active
                    .as_ref()
                    .is_some_and(|run| now.saturating_sub(run.started_ms) >= RUN_LEASE)
                {
                    status.active = None;
                    status.reset_confirmation();
                    status.last_status = "interrupted".into();
                }
                if status.confirm_due_ms.is_some_and(|due| {
                    due <= now
                        && (state.settings.release(now, status) > now
                            || !state
                                .settings
                                .confirmation_current(due, status.next_due_ms, now))
                }) {
                    // 维护中断跨过禁测时段或常规周期，不使用陈旧结果自动停用。
                    status.reset_confirmation();
                }
            }
            let due = if state.paused {
                None
            } else {
                state
                    .settings
                    .account_ids
                    .iter()
                    .filter_map(|id| {
                        let status = state.accounts.get(id)?;
                        let at = status.confirm_due_ms.unwrap_or(status.next_due_ms);
                        (!status.paused
                            && status.active.is_none()
                            && at <= now
                            && state.settings.release(now, status) == now)
                            .then_some((id.clone(), at))
                    })
                    .min_by_key(|(_, at)| *at)
                    .map(|(id, _)| id)
            };
            let Some(id) = due else {
                store::put(host, "monitor", &state, version).await?;
                return Ok(());
            };
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
            let job = (
                id,
                request_id,
                state.settings.model.clone(),
                state.generation.clone(),
                state.settings.client_key_id.clone(),
            );
            store::put(host, "monitor", &state, version).await?;
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
        // 复测通过下一次维护回调预约，不在30秒父调用中sleep，也不脱离父调用发请求。
        let result = app.run(host, request, 18, Some(job.3.clone()), true).await;
        if let Err(error) = result {
            let _guard = self.lock.lock().await;
            let (mut state, version) = self.load(host).await?;
            if let Some(status) = state.accounts.get_mut(&job.0)
                && status.active.as_ref().is_some_and(|run| run.id == job.1)
            {
                status.active = None;
                status.reset_confirmation();
                status.last_status = "skipped".into();
                status.last_checked_ms = Some(now_ms());
                state.last_error = Some(error);
                store::put(host, "monitor", &state, version).await?;
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
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
