use crate::questions::Question;
use gateway_plugin_sdk::{
    call::host::{StateGetRequest, StateGetResult, StatePutRequest},
    client::HostClient,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

pub async fn payload_call<T: DeserializeOwned>(
    host: &HostClient,
    method: &str,
    body: &impl Serialize,
) -> Result<T, String> {
    let payload = serde_json::to_vec(body).map_err(|_| "请求编码失败")?;
    let reply = host
        .call(method, json!({}), payload)
        .await
        .map_err(|_| "宿主调用未完成，请核对权限与运行状态")?;
    serde_json::from_slice(&reply.payload).map_err(|_| "宿主响应格式无效".into())
}

pub async fn metadata_call<T: DeserializeOwned>(
    host: &HostClient,
    method: &str,
    body: &impl Serialize,
) -> Result<T, String> {
    let params = serde_json::to_value(body).map_err(|_| "请求编码失败")?;
    let reply = host
        .call(method, params, vec![])
        .await
        .map_err(|_| "宿主调用未完成，请核对权限与运行状态")?;
    serde_json::from_value(reply.result).map_err(|_| "宿主响应格式无效".into())
}

pub async fn get<T: DeserializeOwned + Default>(
    host: &HostClient,
    key: &str,
) -> Result<(T, Option<u64>), String> {
    let reply = host
        .call(
            "host.state.get",
            serde_json::to_value(StateGetRequest {
                namespace: "quality".into(),
                key: key.into(),
            })
            .map_err(|_| "状态请求编码失败")?,
            vec![],
        )
        .await
        .map_err(|_| "历史状态读取失败，未继续执行")?;
    let result: StateGetResult =
        serde_json::from_value(reply.result).map_err(|_| "历史状态响应无效")?;
    match result.record {
        None => Ok((T::default(), None)),
        Some(record) => Ok((
            serde_json::from_value(record.value).map_err(|_| "历史状态内容无效")?,
            Some(record.version),
        )),
    }
}

pub async fn put(
    host: &HostClient,
    key: &str,
    value: &impl Serialize,
    version: Option<u64>,
) -> Result<(), String> {
    let request = StatePutRequest {
        namespace: "quality".into(),
        key: key.into(),
        value: serde_json::to_value(value).map_err(|_| "状态编码失败")?,
        expected_version: version,
    };
    host.call(
        "host.state.put",
        serde_json::to_value(request).map_err(|_| "状态请求编码失败")?,
        vec![],
    )
    .await
    .map_err(|_| "状态写入未确认，请查询历史后再操作")?;
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomBank {
    pub questions: Vec<Question>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub batch_id: String,
    pub account_id: String,
    pub account_name: String,
    pub mode: String,
    pub model: String,
    pub effort: String,
    pub question_id: Option<String>,
    pub question_title: Option<String>,
    pub prompt: Option<String>,
    pub expected: Option<String>,
    pub status: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub latency_ms: Option<u64>,
    pub answer: Option<String>,
    pub detail: String,
    pub metrics: Value,
}

#[derive(Default, Serialize, Deserialize)]
pub struct History {
    pub records: Vec<Record>,
    pub evicted: u64,
}

impl History {
    pub fn append(&mut self, record: Record) -> Result<(), String> {
        self.records.push(record);
        self.compact(now_ms())
    }

    pub fn compact(&mut self, now: u64) -> Result<(), String> {
        // 宿主父调用最长120秒，过期未确认项保留状态，但不再永久占用容量。
        // 新调用先为最多4个在途答案预留空间，完成回写不淘汰自己的记录。
        while self.records.len() > 100
            || serde_json::to_vec(self).map_err(|_| "历史编码失败")?.len() > 195_000
        {
            let index = self
                .records
                .iter()
                .position(|r| {
                    r.status != "running" || now.saturating_sub(r.started_at_ms) >= 150_000
                })
                .ok_or("历史容量不足，请等待当前测试完成")?;
            self.records.remove(index);
            self.evicted += 1;
        }
        Ok(())
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retention_preserves_pending() {
        let mut history = History::default();
        for i in 0..102 {
            history
                .append(Record {
                    id: i.to_string(),
                    batch_id: "batch".into(),
                    account_id: "acct".into(),
                    account_name: "name".into(),
                    mode: "probe".into(),
                    model: "test".into(),
                    effort: "default".into(),
                    question_id: None,
                    question_title: None,
                    prompt: None,
                    expected: None,
                    status: if i == 0 { "running" } else { "healthy" }.into(),
                    started_at_ms: now_ms(),
                    finished_at_ms: None,
                    latency_ms: None,
                    answer: None,
                    detail: String::new(),
                    metrics: json!({}),
                })
                .unwrap();
        }
        assert_eq!(history.records.len(), 100);
        assert_eq!(history.records[0].id, "0");
        assert_eq!(history.evicted, 2);
    }
}
