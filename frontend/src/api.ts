export interface Question { id: string; title: string; category: string; prompt: string; answer: string }
export interface Account { id: string; name: string; email?: string | null; provider: string; authentication_kind: string; enabled: boolean; probe_supported: boolean; proxy_configured: boolean }
export interface Key { id: string; name: string; enabled: boolean }
export interface Catalog { accounts: Account[]; keys: Key[]; questions: Question[]; bank_version: number | null; version: string }
export interface TestRecord {
  id: string; batch_id: string; account_id: string; account_name: string; mode: string; model: string; effort: string
  question_id: string | null; question_title: string | null; prompt: string | null; expected: string | null
  status: string; started_at_ms: number; finished_at_ms: number | null; latency_ms: number | null
  answer: string | null; detail: string; metrics: Record<string, unknown>
}
export interface History { records: TestRecord[]; evicted: number }
export interface RunInput {
  id: string; batch_id: string; account_id: string; mode: string; model: string; effort: string
  client_key_id: string | null; question_id: string | null
}
interface Bridge {
  version: number; theme: 'light' | 'dark'
  request(input: { method: string; path: string; contentType?: string; body?: ArrayBuffer }): Promise<{ status: number; body: ArrayBuffer }>
}
declare global { interface Window { readonly codexProxyPlugin?: Bridge } }

export async function request<T>(path: string, body?: unknown): Promise<T> {
  const bridge = window.codexProxyPlugin
  if (!bridge) throw new Error('请从CPR插件管理页面打开')
  const bytes = body === undefined ? undefined : new TextEncoder().encode(JSON.stringify(body)).buffer
  const response = await bridge.request({ method: body === undefined ? 'GET' : 'POST', path, ...(bytes ? { contentType: 'application/json', body: bytes } : {}) })
  let value: T & { error?: string }
  try { value = JSON.parse(new TextDecoder().decode(response.body)) } catch { throw new Error('插件返回了无效响应') }
  if (response.status < 200 || response.status >= 300) throw new Error(value.error || `插件请求失败（${response.status}）`)
  return value
}
