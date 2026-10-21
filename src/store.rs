use crate::questions::Question;
use gateway_plugin_sdk::{
    Frame, Message,
    call::host::{StateGetRequest, StateGetResult, StatePutRequest},
    client::{HostClient, validate_frame},
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

// 协议5沿用业务JSON的payload前缀；沿用60KiB业务预算，避免升级时无故淘汰策略变化。
pub const STATE_FRAME_BUDGET: usize = 60 * 1024;
const PENDING_RESULT_RESERVE: usize = 1024;

pub fn state_metadata_bytes(key: &str, value: &impl Serialize) -> Result<usize, String> {
    let params = serde_json::to_value(StatePutRequest {
        namespace: "quality".into(),
        key: key.into(),
        value: serde_json::to_value(value).map_err(|_| "状态编码失败")?,
        // 使用最长ID和版本计算预算，不能依赖当前小整数序号。
        expected_version: Some(u64::MAX),
    })
    .map_err(|_| "状态请求编码失败")?;
    // 不能序列化Message估算：当前协议的params/result带serde(skip)，会漏掉全部业务值。
    serde_json::to_vec(&json!({"type":"callback","id":u64::MAX,"parent_id":u64::MAX,"method":"host.state.put","params":params}))
        .map(|bytes| bytes.len())
        .map_err(|_| "状态请求编码失败".into())
}

pub async fn put(
    host: &HostClient,
    key: &str,
    value: &impl Serialize,
    version: Option<u64>,
) -> Result<(), String> {
    if state_metadata_bytes(key, value)? > STATE_FRAME_BUDGET {
        return Err(
            "状态内容超过60KiB通信预算，未写入；请缩减题库或监控范围，历史由插件自动控制容量"
                .into(),
        );
    }
    let request = StatePutRequest {
        namespace: "quality".into(),
        key: key.into(),
        value: serde_json::to_value(value).map_err(|_| "状态编码失败")?,
        expected_version: version,
    };
    let params = serde_json::to_value(request).map_err(|_| "状态请求编码失败")?;
    validate_frame(&Frame::control(Message::Callback {
        id: u64::MAX,
        parent_id: u64::MAX,
        method: "host.state.put".into(),
        params: params.clone(),
    }))
    .map_err(|_| "状态内容超过SDK消息帧容量，未写入")?;
    host.call("host.state.put", params, vec![])
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

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct History {
    pub records: Vec<Record>,
    pub evicted: u64,
}

impl History {
    pub fn append(&mut self, record: Record) -> Result<(), String> {
        let id = record.id.clone();
        self.records.push(record);
        self.compact(now_ms(), &id)
    }

    pub fn finish(&mut self, mut record: Record, now: u64) -> Result<Record, String> {
        let existing = self
            .records
            .iter()
            .position(|r| r.id == record.id)
            .ok_or("测试记录丢失，结果未保存")?;
        let mut candidate = self.clone();
        candidate.records[existing] = record.clone();
        if candidate.compact(now, &record.id).is_ok() {
            *self = candidate;
            return Ok(record);
        }
        // 单项结果及其他在途凭证本身已超预算，明确记录容量失败，不截断后冒充成功。
        let initial = self.records[existing].clone();
        record = Record {
            status: if record.mode == "probe" {
                "inconclusive"
            } else {
                "failed"
            }
            .into(),
            finished_at_ms: record.finished_at_ms,
            latency_ms: record.latency_ms,
            answer: None,
            detail: "测试结果超出历史通信容量，答案与指标未保存，未自动重试".into(),
            metrics: json!({"client_key_id":record.metrics.get("client_key_id"),"source":record.metrics.get("source"),"storage_error":"result_too_large"}),
            ..initial
        };
        self.records[existing] = record.clone();
        self.compact(now, &record.id)?;
        Ok(record)
    }

    fn compact(&mut self, now: u64, protected_id: &str) -> Result<(), String> {
        loop {
            let pending = self
                .records
                .iter()
                .filter(|r| r.status == "running" && now.saturating_sub(r.started_at_ms) < 150_000)
                .count();
            // 初始凭证与完成结果都检查真实封装字节，为在途操作的容量失败终态预留空间。
            let finalizing = usize::from(
                self.records
                    .iter()
                    .any(|r| r.id == protected_id && r.status != "running"),
            );
            let budget =
                STATE_FRAME_BUDGET.saturating_sub((pending + finalizing) * PENDING_RESULT_RESERVE);
            if self.records.len() <= 100 && state_metadata_bytes("history", self)? <= budget {
                return Ok(());
            }
            let index = self
                .records
                .iter()
                .position(|r| {
                    r.id != protected_id
                        && (r.status != "running" || now.saturating_sub(r.started_at_ms) >= 150_000)
                })
                .ok_or("历史容量不足，请等待在途测试结束；本次未发送上游")?;
            self.records.remove(index);
            self.evicted = self.evicted.saturating_add(1);
        }
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
    fn record(id: &str, status: &str, now: u64) -> Record {
        Record {
            id: id.into(),
            batch_id: "batch".into(),
            account_id: id.into(),
            account_name: "测试账号".into(),
            mode: "question".into(),
            model: "model".into(),
            effort: "medium".into(),
            question_id: Some("candy".into()),
            question_title: None,
            prompt: None,
            expected: None,
            status: status.into(),
            started_at_ms: now,
            finished_at_ms: None,
            latency_ms: None,
            answer: None,
            detail: String::new(),
            metrics: json!({}),
        }
    }
    #[test]
    fn full_envelope_counts_utf8_escaping_and_largest_identifiers() {
        let plain = json!({"text":"x".repeat(10000)});
        let escaped = json!({"text":"\0".repeat(10000)});
        assert!(
            state_metadata_bytes("history", &escaped).unwrap()
                > state_metadata_bytes("history", &plain).unwrap() + 40000
        );
        assert!(
            state_metadata_bytes("history", &json!({"text":"中".repeat(1000)})).unwrap() > 3000
        );
        assert!(state_metadata_bytes("history", &json!({})).unwrap() > 150);
    }
    #[tokio::test]
    async fn protocol_five_business_json_and_binary_payload_round_trip() {
        use gateway_plugin_sdk::client::{read_frame, write_frame};
        let value = json!({"text":"x".repeat(70 * 1024)});
        let frame = Frame {
            message: Message::Callback {
                id: 7,
                parent_id: 9,
                method: "host.state.put".into(),
                params: value.clone(),
            },
            payload: b"fixture-binary".to_vec(),
        };
        let mut wire = Vec::new();
        write_frame(&mut wire, &frame).await.unwrap();
        let metadata_len = u32::from_be_bytes(wire[..4].try_into().unwrap()) as usize;
        assert!(metadata_len < 1024);
        let metadata: Value = serde_json::from_slice(&wire[12..12 + metadata_len]).unwrap();
        assert!(metadata.get("params").is_none());
        assert_eq!(
            metadata["data_bytes"],
            serde_json::to_vec(&value).unwrap().len()
        );
        let decoded = read_frame(&mut wire.as_slice()).await.unwrap();
        match decoded.message {
            Message::Callback { params, .. } => assert_eq!(params, value),
            _ => panic!("unexpected frame"),
        }
        assert_eq!(decoded.payload, b"fixture-binary");
    }
    #[test]
    fn sdk_frames_accept_reserved_put_and_get_envelopes() {
        let value = json!({"text":"x".repeat(STATE_FRAME_BUDGET - 1024)});
        let put = Frame::control(Message::Callback {
            id: u64::MAX,
            parent_id: u64::MAX,
            method: "host.state.put".into(),
            params: serde_json::to_value(StatePutRequest {
                namespace: "quality".into(),
                key: "history".into(),
                value: value.clone(),
                expected_version: Some(u64::MAX),
            })
            .unwrap(),
        });
        let get = Frame::control(Message::Result {
            id: u64::MAX,
            result: json!({"record":{"value":value,"version":u64::MAX,"schema_version":u32::MAX}}),
        });
        assert!(state_metadata_bytes("history", &value).unwrap() <= STATE_FRAME_BUDGET);
        assert!(validate_frame(&put).is_ok());
        assert!(validate_frame(&get).is_ok());
    }
    #[test]
    fn finishing_compacts_old_history_and_protects_all_inflight_records() {
        let now = now_ms();
        let mut history = History::default();
        for i in 0..40 {
            let mut old = record(&format!("old-{i}"), "correct", now);
            old.answer = Some("x".repeat(1500));
            history.records.push(old);
        }
        for i in 0..4 {
            history
                .append(record(&format!("pending-{i}"), "running", now))
                .unwrap();
        }
        for i in 0..4 {
            let mut result = record(&format!("pending-{i}"), "correct", now);
            result.answer = Some("\\\n\"".repeat(2500));
            result.finished_at_ms = Some(now + 1000);
            let finished = history.finish(result, now + 1000).unwrap();
            assert_eq!(finished.status, "correct");
            assert!(history.records.iter().any(|r| r.id == finished.id));
            assert!(state_metadata_bytes("history", &history).unwrap() <= STATE_FRAME_BUDGET);
            for remaining in i + 1..4 {
                assert!(
                    history
                        .records
                        .iter()
                        .any(|r| r.id == format!("pending-{remaining}") && r.status == "running")
                );
            }
        }
        assert!(history.evicted > 0);
    }
    #[test]
    fn oversized_result_is_explicit_terminal_failure_not_stuck_or_truncated_success() {
        let now = now_ms();
        let mut history = History::default();
        history.append(record("operation", "running", now)).unwrap();
        let mut result = record("operation", "correct", now);
        result.answer = Some("\0".repeat(12000));
        result.finished_at_ms = Some(now + 1);
        result.latency_ms = Some(1);
        let result = history.finish(result, now + 1).unwrap();
        assert_eq!(result.status, "failed");
        assert!(result.answer.is_none());
        assert_eq!(result.metrics["storage_error"], "result_too_large");
        assert_eq!(result.finished_at_ms, Some(now + 1));
        assert_eq!(history.records[0].status, "failed");
        assert!(state_metadata_bytes("history", &history).unwrap() <= STATE_FRAME_BUDGET);
    }
    #[test]
    fn append_cannot_discard_current_or_active_operations() {
        let now = now_ms();
        let mut history = History::default();
        for i in 0..4 {
            let mut current = record(&i.to_string(), "running", now);
            current.prompt = Some("x".repeat(13000));
            history.append(current).unwrap();
        }
        let mut current = record("next", "running", now);
        current.prompt = Some("x".repeat(13000));
        assert!(history.append(current).is_err());
        for i in 0..4 {
            assert!(history.records.iter().any(|r| r.id == i.to_string()));
        }
    }
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
