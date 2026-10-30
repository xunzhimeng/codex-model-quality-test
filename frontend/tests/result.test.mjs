import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import ts from 'typescript'
const require = createRequire(import.meta.url)
function load(source, imports = {}, globals = {}) {
  const code = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText
  const exports = {}
  new Function('require', 'exports', ...Object.keys(globals), code)(name => imports[name] || require(name), exports, ...Object.values(globals))
  return exports
}
const result = load(readFileSync(new URL('../src/result.ts', import.meta.url), 'utf8'))
const { resultView, failureLabel, retryAfterSummary, failureHints, attemptSummary, attemptGroups } = result
const failure = (kind, fields = {}) => ({ kind, message: '安全错误摘要', retryable: false, ...fields })
const record = (metrics = {}, fields = {}) => ({ status: 'inconclusive', detail: '', metrics, ...fields })
const round = (attempt, fields = {}) => ({ attempt, phase: '首轮获取门票', http_status: 200, elapsed_ms: 10, completed: true, response_bytes: 100, ticket_present: true, ticket_length: 12, routing_cookie_count: 1, reported_model: null, error: null, ...fields })

test('所有结构化失败显示具体分类并共用错误颜色', () => {
  const labels = {
    timeout: '请求超时', connection_error: '连接失败', read_error: '响应读取失败', rate_limited: '上游限流',
    quota_exhausted: '额度耗尽', authentication: '认证失败', permission_denied: '权限不足', model_unavailable: '模型不可用',
    invalid_request: '请求无效', upstream_error: '上游错误', stream_error: '响应流失败', invalid_response: '响应无效',
    missing_ticket: '未返回门票', preparation: '探针准备失败',
  }
  for (const [kind, label] of Object.entries(labels)) {
    const view = resultView(record({ failure: failure(kind) }))
    assert.equal(view.label, label, kind)
    assert.equal(view.className, 'failed', kind)
  }
})

test('HTTP状态与错误分类均保留，不把额度错误误标成限流', () => {
  for (const [http_status, kind, reason] of [[429, 'rate_limited', '上游限流'], [429, 'quota_exhausted', '额度耗尽'], [401, 'authentication', '认证失败'], [403, 'permission_denied', '权限不足'], [500, 'upstream_error', '上游错误'], [503, 'upstream_error', '上游错误']]) {
    assert.equal(resultView(record({ failure: failure(kind, { http_status }) })).label, `HTTP ${http_status} · ${reason}`)
  }
  assert.equal(failureLabel(failure('future_kind', { message: '安全的新分类', http_status: 502 })), 'HTTP 502 · 安全的新分类')
  assert.equal(failureLabel(failure('stream_error', { http_status: 200 })), '响应流失败')
})

test('healthy与degraded保留原标签和颜色，包括前一次尝试曾失败的记录', () => {
  const metrics = { failure: failure('timeout'), rounds: [round(1, { http_status: 429 })] }
  assert.deepEqual(resultView(record(metrics, { status: 'healthy' })), { label: '未发现降级', className: 'healthy', attempts: '' })
  assert.deepEqual(resultView(record(metrics, { status: 'degraded' })), { label: '疑似降级', className: 'degraded', attempts: '' })
  assert.equal(resultView({ status: 'completed' }).label, '已完成')
})

test('旧探针从rounds与安全detail识别HTTP、超时、连接、门票和准备失败', () => {
  for (const status of [401, 403, 429, 500, 503]) {
    assert.equal(resultView(record({ rounds: [round(undefined), round(undefined, { http_status: status })] })).label, `HTTP ${status}`)
    assert.equal(resultView(record({}, { detail: `上游拒绝探针请求（HTTP ${status}），无法判断` })).label, `HTTP ${status} · 上游拒绝探针请求`)
  }
  assert.equal(resultView(record({}, { detail: '探针超时，无法判断，未自动重试' })).label, '请求超时')
  assert.equal(resultView(record({ rounds: [round(undefined, { http_status: null, error: '账号代理出站失败，禁止回退直连' })] })).label, '连接失败')
  assert.equal(resultView(record({}, { detail: '首轮未返回门票，无法判断' })).label, '首轮未返回门票')
  assert.equal(resultView(record({}, { detail: '账号未配置代理，无法判断' })).label, '账号未配置代理')
  assert.equal(resultView(record({}, { detail: '上游响应未完整完成，无法判断' })).label, '上游响应未完整完成')
})

test('题目失败只从安全detail提取HTTP，没有HTTP仍显示调用失败', () => {
  for (const status of [429, 401, 403, 500, 502, 503]) {
    assert.equal(resultView({ status: 'failed', detail: `宿主模型调用失败（HTTP ${status}，code: fixture_safe_code）` }).label, `HTTP ${status}`)
  }
  assert.equal(resultView({ status: 'failed', detail: '宿主模型调用失败（code: fixture_safe_code）' }).label, '调用失败')
  assert.equal(resultView({ status: 'failed', detail: '宿主调用超时' }).label, '调用失败')
  assert.equal(resultView({ status: 'failed', detail: 'http429' }).label, 'HTTP 429')
  assert.equal(resultView({ status: 'wrong', detail: 'HTTP 429' }).label, '答错')
})

test('无证据的旧记录与无last_result的监控不伪造失败或尝试次数', () => {
  for (const metrics of [undefined, null, {}, { rounds: [] }, { rounds: [round(undefined)] }]) {
    assert.deepEqual(resultView(record(metrics, { detail: '无法判断' })), { label: '未取得判定', className: 'inconclusive', attempts: '' })
    assert.equal(attemptSummary(metrics), '')
  }
  assert.deepEqual(resultView({ status: 'inconclusive' }), { label: '未取得判定', className: 'inconclusive', attempts: '' })
  assert.equal(resultView({ status: 'running' }).label, '结果未确认')
  assert.equal(resultView({ status: 'skipped' }).label, '未执行')
  assert.equal(resultView({ status: 'interrupted' }).label, '执行中断')
  assert.equal(resultView({ status: '' }).label, '尚未执行')
})

test('次数摘要使用真实尝试数，并覆盖全部停止重试原因', () => {
  const reasons = { not_retryable: '错误不可重试', max_attempts: '达到最大尝试次数', budget_exhausted: '重试预算耗尽', retry_after_exceeds_budget: '等待时间超出重试预算' }
  for (const [retry_stop, reason] of Object.entries(reasons)) {
    const metrics = { attempt_count: 2, retry_stop }
    assert.equal(attemptSummary(metrics), `共 2 次尝试 · 停止重试：${reason}`)
    assert.equal(resultView(record(metrics)).attempts, attemptSummary(metrics))
  }
  assert.equal(attemptSummary({ attempt_count: 1, retry_stop: null }), '共 1 次尝试')
  assert.equal(attemptSummary({ attempt_count: 1, retry_stop: 'not_retryable' }), '共 1 次尝试 · 停止重试：错误不可重试')
  assert.equal(attemptSummary({ attempts: [{ number: 1 }, { number: 2 }] }), '共 2 次尝试')
  assert.equal(attemptSummary({ rounds: [round(undefined), round(undefined)] }), '')
})

test('Retry-After摘要显示建议等待秒数，保留0秒并拒绝不安全数值', () => {
  for (const [retry_after_ms, seconds] of [[0, 0], [250, 0.25], [1500, 1.5], [120000, 120]]) {
    const value = failure('rate_limited', { retry_after_ms })
    assert.equal(retryAfterSummary(value), `建议等待 ${seconds} 秒`)
    assert.equal(attemptSummary({ attempt_count: 1, retry_stop: 'retry_after_exceeds_budget', failure: value }), `共 1 次尝试 · 停止重试：等待时间超出重试预算 · 建议等待 ${seconds} 秒`)
    assert.ok(resultView(record({ failure: value })).attempts.includes(`建议等待 ${seconds} 秒`))
  }
  assert.ok(retryAfterSummary(failure('rate_limited', { retry_after_ms: Number.MAX_SAFE_INTEGER })).startsWith('建议等待 '))
  for (const retry_after_ms of [undefined, null, -1, 0.5, NaN, Infinity, -Infinity, Number.MAX_SAFE_INTEGER + 1, '120000']) {
    assert.equal(retryAfterSummary(failure('rate_limited', { retry_after_ms })), '')
  }
  assert.equal(retryAfterSummary(null), '')
})

test('失败附加信息显示安全错误码与等待建议，摘要已有时不重复', () => {
  assert.equal(failureHints(failure('rate_limited', { code: 'rate_limit_exceeded', retry_after_ms: 120000 })), '建议等待 120 秒 · 错误码：rate_limit_exceeded')
  assert.equal(failureHints(failure('rate_limited', { code: 'rate_limit_exceeded', retry_after_ms: 0, message: 'rate_limit_exceeded，建议等待 0 秒' })), '')
  assert.equal(failureHints(failure('rate_limited', { code: 'rate_limit_exceeded', message: 'rate_limit_exceeded' })), '')
  assert.equal(failureHints(failure('timeout', { code: null, retry_after_ms: null })), '')
})

test('重复phase按attempt归组，旧轮次不猜测所属尝试', () => {
  const attempts = [{ number: 1, status: 'failed', elapsed_ms: 20, failure: failure('timeout') }, { number: 2, status: 'completed', elapsed_ms: 30 }]
  const rounds = [round(1), round(1, { phase: '携带门票续接' }), round(2), round(2, { phase: '携带门票续接' })]
  const groups = attemptGroups({ attempts, rounds })
  assert.deepEqual(groups.map(group => group.number), [1, 2])
  assert.deepEqual(groups.map(group => group.rounds.length), [2, 2])
  assert.equal(groups[0].attempt.failure.kind, 'timeout')
  assert.equal(groups[1].rounds[0], rounds[2])
  const legacy = attemptGroups({ rounds: [round(undefined), round(undefined)] })
  assert.equal(legacy[0].number, null)
  assert.equal(legacy[0].attempt, null)
  assert.equal(legacy[0].rounds.length, 2)
  assert.equal(attemptGroups({ attempts, rounds: [round(undefined)] })[2].number, null)
  assert.deepEqual(attemptGroups({}), [])
})

const { parse, compileScript } = require('vue/compiler-sfc')
const { createSSRApp, h } = require('vue')
const { renderToString } = require('vue/server-renderer')
const { descriptor } = parse(readFileSync(new URL('../src/ProbeDetails.vue', import.meta.url), 'utf8'))
const ProbeDetails = load(compileScript(descriptor, { id: 'probe-details-test', inlineTemplate: true }).content, { './result': result }).default

test('详情实际渲染尝试分组、各次失败与停止原因，只显示门票存在性及长度', async () => {
  const html = await renderToString(createSSRApp({ render: () => h(ProbeDetails, { metrics: {
    attempt_count: 2, retry_stop: 'max_attempts',
    attempts: [{ number: 1, status: 'failed', elapsed_ms: 10, failure: failure('rate_limited', { http_status: 429, message: '安全限流摘要' }) }, { number: 2, status: 'failed', elapsed_ms: 20, failure: failure('timeout', { message: '安全超时摘要' }) }],
    rounds: [round(1, { http_status: 429 }), round(2, { http_status: null, ticket_present: false, ticket_length: 0 })],
    ticket: 'fixture-ticket-should-not-render', cookie: 'fixture-cookie-should-not-render',
  } }) }))
  for (const text of ['共 2 次尝试', '达到最大尝试次数', '第 1 次尝试', '第 2 次尝试', 'HTTP 429 · 上游限流', '安全限流摘要', '请求超时', '安全超时摘要', '已返回 · 12 字节']) assert.ok(html.includes(text), text)
  assert.doesNotMatch(html, /fixture-ticket-should-not-render|fixture-cookie-should-not-render|无法判断/)
})

test('详情可见最终失败及历史尝试的Retry-After建议和安全错误码', async () => {
  const finalFailure = failure('rate_limited', { http_status: 429, retry_after_ms: 120000, code: 'rate_limit_exceeded' })
  const metrics = {
    failure: finalFailure, attempt_count: 2, retry_stop: 'retry_after_exceeds_budget',
    attempts: [{ number: 1, status: 'failed', elapsed_ms: 10, failure: failure('rate_limited', { http_status: 429, retry_after_ms: 0, code: 'turn_rate_limit_exceeded' }) }, { number: 2, status: 'failed', elapsed_ms: 20, failure: finalFailure }],
  }
  const html = await renderToString(createSSRApp({ render: () => h(ProbeDetails, { metrics }) }))
  for (const text of ['建议等待 120 秒', '建议等待 0 秒', '错误码：rate_limit_exceeded', '错误码：turn_rate_limit_exceeded', '等待时间超出重试预算']) assert.ok(html.includes(text), text)
  const unlisted = await renderToString(createSSRApp({ render: () => h(ProbeDetails, { metrics: { failure: finalFailure } }) }))
  assert.ok(unlisted.includes('建议等待 120 秒'))
  assert.ok(unlisted.includes('错误码：rate_limit_exceeded'))
})

test('详情兼容无尝试信息的旧记录与准备阶段失败', async () => {
  const legacy = await renderToString(createSSRApp({ render: () => h(ProbeDetails, { metrics: { rounds: [round(undefined)] } }) }))
  assert.ok(legacy.includes('首轮获取门票'))
  assert.doesNotMatch(legacy, /第 1 次尝试|共 1 次尝试/)
  const preparation = await renderToString(createSSRApp({ render: () => h(ProbeDetails, { metrics: { failure: failure('preparation', { message: '账号未配置代理' }), attempt_count: 1, retry_stop: 'not_retryable' } }) }))
  for (const text of ['探针准备失败', '账号未配置代理', '共 1 次尝试', '错误不可重试']) assert.ok(preparation.includes(text), text)
})

function bindings(file, globals = {}) {
  const vue = require('vue')
  const models = { useModels: () => ({ models: vue.ref([]), modelsLoading: vue.ref(false), modelsError: vue.ref(''), loadModels: async () => {} }) }
  const { descriptor } = parse(readFileSync(new URL(`../src/${file}`, import.meta.url), 'utf8'))
  const queue = load(readFileSync(new URL('../src/queue.ts', import.meta.url), 'utf8'))
  const component = load(compileScript(descriptor, { id: 'result-bindings-test' }).content, {
    vue: { ...vue, onMounted: () => {} }, './api': { request: async () => { throw new Error('测试不应发起请求') } },
    './result': result, './queue': queue, './models': models, './MonitorPanel.vue': {}, './ProbeDetails.vue': {},
  }, globals).default
  return component.setup({ accounts: [], keys: [] }, { expose: () => {} })
}

test('监控复用last_result的标签与次数，无保留历史时只显示旧状态', () => {
  const { accountResult } = bindings('MonitorPanel.vue')
  assert.deepEqual(accountResult({ last_status: 'inconclusive', last_result: record({ failure: failure('authentication', { http_status: 401 }), attempt_count: 1, retry_stop: 'not_retryable' }) }), {
    label: 'HTTP 401 · 认证失败', className: 'failed', attempts: '共 1 次尝试 · 停止重试：错误不可重试',
  })
  assert.deepEqual(accountResult({ last_status: 'inconclusive', last_result: null }), { label: '未取得判定', className: 'inconclusive', attempts: '' })
  assert.equal(accountResult({ last_status: 'healthy' }).label, '未发现降级')
  assert.equal(accountResult({ last_status: 'degraded' }).label, '疑似降级')
  assert.equal(accountResult(undefined).label, '尚未执行')
})

test('CSV导出沿用统一错误标签并保留现有安全detail', async () => {
  let blob, clicked = false
  const app = bindings('App.vue', {
    URL: { createObjectURL: value => { blob = value; return 'blob:fixture' }, revokeObjectURL: () => {} },
    document: { createElement: () => ({ click: () => { clicked = true } }) }, setTimeout: () => {},
  })
  const base = { id: 'record', batch_id: 'batch', account_id: 'account', account_name: 'fixture-account', mode: 'probe', model: 'fixture-model', effort: 'default', started_at_ms: 1, latency_ms: 10, question_title: null, answer: null, expected: null }
  app.records.value = [
    { ...base, ...record({ failure: failure('rate_limited', { http_status: 429 }) }, { detail: '安全限流摘要，未取得降级判定' }) },
    { ...base, ...record({}, { status: 'failed', detail: '宿主模型调用失败（HTTP 403，code: permission_denied）' }), mode: 'question' },
  ]
  app.tab.value = 'history'
  assert.equal(resultView(app.comparison.value[0].probe).label, 'HTTP 429 · 上游限流')
  app.download('csv')
  const csv = await blob.text()
  assert.equal(clicked, true)
  assert.ok(csv.includes('HTTP 429 · 上游限流'))
  assert.ok(csv.includes('安全限流摘要，未取得降级判定'))
  assert.ok(csv.includes('HTTP 403'))
  assert.ok(csv.includes('宿主模型调用失败（HTTP 403，code: permission_denied）'))
})

test('帮助区分探针内部重试、题目不重试及停止后续任务', () => {
  const app = readFileSync(new URL('../src/App.vue', import.meta.url), 'utf8')
  const monitor = readFileSync(new URL('../src/MonitorPanel.vue', import.meta.url), 'utf8')
  for (const text of ['含初次最多3次', '总计最多90秒，每次最多30秒', '题目测试通过宿主Key权限、租约与计费链，不自动重试', '已停止后续任务，已发送的请求不撤回']) assert.ok(app.includes(text), text)
  assert.doesNotMatch(app, /已发送的测试不会自动重试/)
  for (const text of ['整轮计1次', '含初次最多3次', '后台总预算180秒，每次最多60秒', '不撤回已发请求', '超时或错误不触发停用']) assert.ok(monitor.includes(text), text)
})
