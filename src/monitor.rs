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
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        if !(5..=1440).contains(&self.interval_minutes) {
            return Err("间隔须为5至1440分钟".into());
        }
        if ((self.scheduled || self.auto_disable) && self.model.is_empty())
            || self.model.trim() != self.model
            || self.model.len() > 128
            || self.model.bytes().any(|b| b.is_ascii_control())
        {
            return Err("探针模型无效".into());
        }
        if (self.scheduled || self.auto_disable)
            && self
                .client_key_id
                .as_ref()
                .is_none_or(|id| id.trim().is_empty() || id.len() > 128)
        {
            return Err("启用监控须选择客户端Key".into());
        }
        if self.account_ids.len() > 50
            || ((self.scheduled || self.auto_disable) && self.account_ids.is_empty())
        {
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
}
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct AccountState {
    pub next_due_ms: u64,
    pub streak: u8,
    pub last_record_id: String,
    pub last_status: String,
    pub last_checked_ms: Option<u64>,
    pub action: String,
    pub action_detail: String,
}
impl AccountState {
    fn observe(&mut self, record: &Record) {
        if self.last_record_id == record.id {
            return;
        }
        self.streak = if record.status == "degraded" {
            self.streak.saturating_add(1).min(2)
        } else {
            0
        };
        self.last_record_id = record.id.clone();
        self.last_status = record.status.clone();
        self.last_checked_ms = record.finished_at_ms;
    }
}
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub probe_version: u32,
    pub settings: Settings,
    pub generation: String,
    pub accounts: BTreeMap<String, AccountState>,
    pub last_tick_ms: Option<u64>,
    pub last_error: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Save {
    pub settings: Settings,
    pub expected_generation: Option<String>,
}

#[derive(Default)]
pub struct Monitor {
    lock: Mutex<()>,
}
impl Monitor {
    // 出站路径与Key身份变更后旧结论不续算，也不能悄悄启用新的网络路径。
    async fn load(&self, host: &HostClient) -> Result<(State, Option<u64>), String> {
        let (mut state, version) = store::get::<State>(host, "monitor").await?;
        if version.is_some() && state.probe_version == 0 {
            state.probe_version = 1;
            state.settings.scheduled = false;
            state.settings.auto_disable = false;
            state.generation = uuid::Uuid::new_v4().to_string();
            for status in state.accounts.values_mut() {
                status.streak = 0;
                status.last_status = "".into();
            }
            state.last_error =
                Some("探针已切换为Key范围校验和账号代理，请选择Key并重新保存启用计划".into());
            store::put(host, "monitor", &state, version).await?;
            return store::get::<State>(host, "monitor").await;
        }
        Ok((state, version))
    }
    pub async fn snapshot(&self, host: &HostClient) -> Result<Value, String> {
        let _guard = self.lock.lock().await;
        let (state, version) = self.load(host).await?;
        Ok(json!({"state":state,"version":version}))
    }
    pub async fn save(&self, host: &HostClient, request: Save) -> Result<Value, String> {
        request.settings.validate()?;
        if request.settings.scheduled || request.settings.auto_disable {
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
        let (previous, version) = self.load(host).await?;
        let current_generation =
            (!previous.generation.is_empty()).then_some(previous.generation.as_str());
        if current_generation != request.expected_generation.as_deref() {
            return Err("监控设置已变化，请刷新后再保存".into());
        }
        let now = now_ms();
        let mut state = State {
            probe_version: 1,
            settings: request.settings,
            generation: uuid::Uuid::new_v4().to_string(),
            ..State::default()
        };
        for id in &state.settings.account_ids {
            let mut status = AccountState {
                next_due_ms: now + u64::from(state.settings.interval_minutes) * 60_000,
                ..AccountState::default()
            };
            // 未确认的停用不可因重保存而重复发送，保留审计结果供人工核对。
            if let Some(old) = previous.accounts.get(id) {
                status.action = old.action.clone();
                status.action_detail = old.action_detail.clone();
            }
            state.accounts.insert(id.clone(), status);
        }
        store::put(host, "monitor", &state, version).await?;
        drop(_guard);
        self.snapshot(host).await
    }
    pub async fn generation(
        &self,
        host: &HostClient,
        account_id: &str,
        model: &str,
        key: &str,
    ) -> Result<Option<String>, String> {
        let _guard = self.lock.lock().await;
        let (state, _) = self.load(host).await?;
        Ok((state.settings.auto_disable
            && state.settings.client_key_id.as_deref() == Some(key)
            && state.settings.model == model
            && state.settings.account_ids.iter().any(|id| id == account_id))
        .then_some(state.generation))
    }
    pub async fn begin(
        &self,
        host: &HostClient,
        id: &str,
        generation: &str,
    ) -> Result<bool, String> {
        let _guard = self.lock.lock().await;
        let (mut state, version) = self.load(host).await?;
        if state.generation != generation {
            return Ok(false);
        }
        if let Some(status) = state.accounts.get_mut(id) {
            // 中断轮次没有完整结论，不能沿用之前的连续命中计数。
            if status.last_status == "running" {
                status.streak = 0;
            }
            status.last_status = "running".into();
        }
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
        if state.generation != generation
            || state.settings.model != record.model
            || state.settings.client_key_id.as_deref()
                != record.metrics.get("client_key_id").and_then(Value::as_str)
        {
            return Ok(());
        }
        let Some(status) = state.accounts.get_mut(&record.account_id) else {
            return Ok(());
        };
        if status.action == "disabled" {
            // 只有已启用账号才能得到新探针结果，意味着管理员已手动恢复。
            status.streak = 0;
            status.action.clear();
            status.action_detail.clear();
        }
        status.observe(record);
        let should_disable =
            state.settings.auto_disable && status.streak >= 2 && status.action.is_empty();
        if should_disable {
            // 先保存动作意图，再修改宿主；响应不明时不得自动重复停用。
            status.action = "pending".into();
            status.action_detail = "停用已请求，等待确认".into();
        }
        record.metrics["monitor"] = json!({"streak":status.streak,"threshold":2,"action":status.action,"detail":status.action_detail});
        store::put(host, "monitor", &state, version).await?;
        if !should_disable {
            return Ok(());
        }
        let result = accounts::disable(host, &record.account_id).await;
        let (mut state, version) = self.load(host).await?;
        if state.generation != generation {
            return Err("停用后监控状态已变化，请核对账号".into());
        }
        if let Some(status) = state.accounts.get_mut(&record.account_id) {
            match result {
                Ok(()) => {
                    status.action = "disabled".into();
                    status.action_detail = "连续两轮疑似降级，账号已停用，需手动恢复".into();
                }
                Err(error) => {
                    status.action = "unconfirmed".into();
                    status.action_detail = error;
                }
            }
            record.metrics["monitor"] = json!({"streak":status.streak,"threshold":2,"action":status.action,"detail":status.action_detail});
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
            let due = state
                .accounts
                .iter()
                .filter(|(_, s)| s.next_due_ms <= now)
                .min_by_key(|(_, s)| s.next_due_ms)
                .map(|(id, _)| id.clone());
            let Some(id) = due else {
                store::put(host, "monitor", &state, version).await?;
                return Ok(());
            };
            state.last_error = None;
            // 先预约下一次到期；重启不补发旧任务，避免宿主重试维护回调产生重复消耗。
            state.accounts.get_mut(&id).unwrap().next_due_ms =
                now + u64::from(state.settings.interval_minutes) * 60_000;
            let job = (
                id,
                state.settings.model.clone(),
                state.generation.clone(),
                state.settings.client_key_id.clone(),
            );
            store::put(host, "monitor", &state, version).await?;
            job
        };
        let request = RunRequest {
            id: uuid::Uuid::new_v4().to_string(),
            batch_id: uuid::Uuid::new_v4().to_string(),
            account_id: job.0.clone(),
            mode: "probe".into(),
            model: job.1,
            effort: "default".into(),
            client_key_id: job.3,
            question_id: None,
        };
        // maintenance父调用30秒：探针最多18秒，为状态读写与停用接口保留时间。
        let result = app.run(host, request, 18, Some(job.2.clone()), true).await;
        let _guard = self.lock.lock().await;
        let (mut state, version) = self.load(host).await?;
        if state.generation != job.2 {
            return Ok(());
        }
        if let Err(error) = result {
            state.last_error = Some(error.clone());
            if let Some(status) = state.accounts.get_mut(&job.0) {
                status.streak = 0;
                status.last_status = "skipped".into();
                status.last_checked_ms = Some(now_ms());
                status.action_detail = if status.action.is_empty() {
                    error
                } else {
                    status.action_detail.clone()
                };
            }
        }
        store::put(host, "monitor", &state, version).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_are_opt_in_and_bounded() {
        let mut settings = Settings::default();
        assert!(!settings.auto_disable && !settings.scheduled);
        assert!(settings.validate().is_ok());
        settings.scheduled = true;
        assert!(settings.validate().is_err());
        settings.account_ids = vec!["account".into()];
        settings.client_key_id = Some("key".into());
        assert!(settings.validate().is_err());
        settings.model = "visible-model".into();
        assert!(settings.validate().is_ok());
        settings.interval_minutes = 0;
        assert!(settings.validate().is_err());
    }
    #[test]
    fn errors_break_consecutive_verdicts_and_replay_does_not_count() {
        let mut s = AccountState::default();
        let mut r:Record=serde_json::from_value(json!({"id":"1","batch_id":"b","account_id":"a","account_name":"n","mode":"probe","model":"m","effort":"default","status":"degraded","started_at_ms":0,"finished_at_ms":1,"detail":"","metrics":{}})).unwrap();
        s.observe(&r);
        s.observe(&r);
        assert_eq!(s.streak, 1);
        r.id = "2".into();
        r.status = "inconclusive".into();
        s.observe(&r);
        assert_eq!(s.streak, 0);
        r.id = "3".into();
        r.status = "degraded".into();
        s.observe(&r);
        r.id = "4".into();
        s.observe(&r);
        assert_eq!(s.streak, 2);
    }
}
