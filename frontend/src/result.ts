export interface Failure {
  kind: string; message: string; http_status?: number | null; code?: string | null
  retryable: boolean; retry_after_ms?: number | null
}
export interface Attempt { number: number; status: string; elapsed_ms: number; failure?: Failure | null }
export interface Round {
  attempt?: number; phase: string; http_status: number | null; elapsed_ms: number; completed: boolean
  response_bytes: number; ticket_present: boolean; ticket_length: number; routing_cookie_count: number
  reported_model: string | null; error: string | null
}
export interface ProbeMetrics {
  failure?: Failure | null; attempt_count?: number; retry_stop?: string | null; attempts?: Attempt[]; rounds?: Round[]
  source?: string; transport?: string; client_key_id?: string; key_billed?: boolean; criterion?: string; new_ticket?: boolean
  monitor?: { task_id?: string; task_name?: string; confirmation?: boolean; streak?: number; threshold?: number; action?: string; detail?: string }
}
interface Result { status: string; detail?: string | null; metrics?: unknown }
interface AttemptGroup { number: number | null; attempt: Attempt | null; rounds: Round[] }
const statusLabels: Record<string, string> = {
  correct: '答对', wrong: '答错', completed: '已完成', failed: '调用失败', running: '结果未确认', skipped: '未执行', interrupted: '执行中断',
  healthy: '未发现降级', degraded: '疑似降级', inconclusive: '未取得判定',
}
const failureLabels: Record<string, string> = {
  timeout: '请求超时', connection_error: '连接失败', read_error: '响应读取失败', rate_limited: '上游限流',
  quota_exhausted: '额度耗尽', authentication: '认证失败', permission_denied: '权限不足', model_unavailable: '模型不可用',
  invalid_request: '请求无效', upstream_error: '上游错误', stream_error: '响应流失败', invalid_response: '响应无效',
  missing_ticket: '未返回门票', preparation: '探针准备失败',
}
const retryStopLabels: Record<string, string> = {
  not_retryable: '错误不可重试', max_attempts: '达到最大尝试次数', budget_exhausted: '重试预算耗尽',
  retry_after_exceeds_budget: '等待时间超出重试预算',
}
function metricsOf(value: unknown): ProbeMetrics { return value && typeof value === 'object' ? value as ProbeMetrics : {} }
function httpLabel(status: unknown) { return typeof status === 'number' && status >= 300 && status <= 599 ? `HTTP ${status}` : '' }
export function failureLabel(failure: Failure) {
  const reason = failureLabels[failure.kind] || failure.message || '调用失败'
  const http = httpLabel(failure.http_status)
  return http ? `${http} · ${reason}` : reason
}
function legacyFailure(result: Result, metrics: ProbeMetrics) {
  // 旧记录只使用已保存的HTTP状态和安全摘要，不根据inconclusive推测失败原因。
  const rounds = Array.isArray(metrics.rounds) ? metrics.rounds : []
  const detailHttp = result.detail?.match(/\bHTTP\s*([3-5]\d{2})\b/i)?.[1]
  const http = [...rounds].reverse().map(round => httpLabel(round.http_status)).find(Boolean)
    || (detailHttp ? `HTTP ${detailHttp}` : '')
  if (result.status === 'failed') return http
  const detail = [result.detail, ...[...rounds].reverse().map(round => round.error)].find(value => value && /超时|timeout|连接失败|出站失败|读取失败|限流|额度|认证|权限|拒绝|未返回门票|缺少|无效|不适用|不支持|未配置|过期|不可用|中断|冲突|超限|超过上限|未完整|未完成|错误|失败|无法(?:读取|连接|用于)/i.test(value))
  let reason = ''
  if (detail) {
    if (/超时|timeout/i.test(detail)) reason = '请求超时'
    else if (/连接失败|出站失败/i.test(detail)) reason = '连接失败'
    else if (/读取失败/.test(detail)) reason = '响应读取失败'
    else reason = detail.replace(/\bHTTP\s*[3-5]\d{2}\b/gi, '').replace(/无法判断|未自动重试/g, '').replace(/[（(][）)]/g, '').replace(/^[\s，,、·：:]+|[\s，,、·：:。；;]+$/g, '').trim()
  }
  return http ? `${http}${reason ? ` · ${reason}` : ''}` : reason
}
export function retryAfterSummary(failure?: Failure | null) {
  const wait = failure?.retry_after_ms
  return typeof wait === 'number' && Number.isSafeInteger(wait) && wait >= 0 ? `建议等待 ${wait / 1000} 秒` : ''
}
export function failureHints(failure: Failure) {
  const wait = retryAfterSummary(failure)
  // code已由后端白名单过滤，只补充安全摘要中尚未包含的值。
  const code = failure.code && !failure.message.includes(failure.code) ? `错误码：${failure.code}` : ''
  return [wait && !failure.message.includes(wait) ? wait : '', code].filter(Boolean).join(' · ')
}
export function attemptSummary(value: unknown) {
  const metrics = metricsOf(value)
  const attempts = Array.isArray(metrics.attempts) ? metrics.attempts : []
  const count = Number.isInteger(metrics.attempt_count) && metrics.attempt_count! >= 0 ? metrics.attempt_count
    : attempts.length ? new Set(attempts.map(attempt => attempt.number)).size : undefined
  const stop = metrics.retry_stop ? retryStopLabels[metrics.retry_stop] : ''
  return [count === undefined ? '' : `共 ${count} 次尝试`, stop ? `停止重试：${stop}` : '', retryAfterSummary(metrics.failure)].filter(Boolean).join(' · ')
}
export function resultView(result: Result) {
  const metrics = metricsOf(result.metrics)
  const settled = ['correct', 'wrong', 'healthy', 'degraded'].includes(result.status)
  const failure = settled ? '' : metrics.failure ? failureLabel(metrics.failure) : legacyFailure(result, metrics)
  return {
    label: failure || statusLabels[result.status] || result.status || '尚未执行',
    className: failure ? 'failed' : result.status === 'completed' ? 'healthy' : result.status,
    attempts: attemptSummary(metrics),
  }
}
export function attemptGroups(value: unknown): AttemptGroup[] {
  const metrics = metricsOf(value), groups = new Map<number | null, AttemptGroup>()
  for (const attempt of Array.isArray(metrics.attempts) ? metrics.attempts : []) {
    groups.set(attempt.number, { number: attempt.number, attempt, rounds: [] })
  }
  for (const round of Array.isArray(metrics.rounds) ? metrics.rounds : []) {
    const number = Number.isInteger(round.attempt) && round.attempt! > 0 ? round.attempt! : null
    if (!groups.has(number)) groups.set(number, { number, attempt: null, rounds: [] })
    groups.get(number)!.rounds.push(round)
  }
  return [...groups.values()].sort((a, b) => (a.number ?? Infinity) - (b.number ?? Infinity))
}
