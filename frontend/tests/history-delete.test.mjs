import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import ts from 'typescript'

const require = createRequire(import.meta.url)
const vue = require('vue')
const { parse, compileScript } = require('vue/compiler-sfc')
const { renderToString } = require('vue/server-renderer')

function load(source, imports = {}, globals = {}) {
  const code = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText
  const exports = {}
  new Function('require', 'exports', ...Object.keys(globals), code)(name => imports[name] || require(name), exports, ...Object.values(globals))
  return exports
}

let nextResponse
const requestCalls = []
const api = { request: async (path, body) => {
  requestCalls.push({ path, body })
  if (nextResponse instanceof Error) throw nextResponse
  return nextResponse
} }
const result = load(readFileSync(new URL('../src/result.ts', import.meta.url), 'utf8'))
const queue = load(readFileSync(new URL('../src/queue.ts', import.meta.url), 'utf8'))
const models = { useModels: () => ({ models: vue.ref([]), modelsLoading: vue.ref(false), modelsError: vue.ref(''), loadModels: async () => {} }) }
const { descriptor } = parse(readFileSync(new URL('../src/App.vue', import.meta.url), 'utf8'))
const App = load(compileScript(descriptor, { id: 'history-delete-test' }).content, {
  vue: { ...vue, onMounted: () => {} },
  './api': api,
  './queue': queue,
  './models': models,
  './result': result,
  './MonitorPanel.vue': {},
  './ProbeDetails.vue': {},
}).default

const { descriptor: probeDescriptor } = parse(readFileSync(new URL('../src/ProbeDetails.vue', import.meta.url), 'utf8'))
const ProbeDetails = load(compileScript(probeDescriptor, { id: 'history-delete-probe-test', inlineTemplate: true }).content, {
  vue,
  './result': result,
}).default

global.document = { activeElement: null }

function record(accountId, accountName, id = `${accountId}-record`) {
  return {
    id, batch_id: 'batch', account_id: accountId, account_name: accountName, mode: 'probe', model: 'model', effort: 'default',
    question_id: null, question_title: null, prompt: null, expected: null, status: 'inconclusive', started_at_ms: 1,
    finished_at_ms: 2, latency_ms: 1, answer: null, detail: '未取得判定', metrics: {},
  }
}

function appWithHistory() {
  const app = App.setup({}, { expose: () => {} })
  app.tab.value = 'history'
  app.catalog.value = { accounts: [{ id: 'catalog-account', name: '目录名称' }], keys: [], questions: [], bank_version: null, version: '' }
  app.records.value = [record('catalog-account', '历史名称'), record('deleted-account', '已删除账号', 'old-1'), record('deleted-account', '旧名称', 'old-2')]
  app.historyVersion.value = 7
  return app
}

test('历史账号下拉去重且目录名称优先，保留已从宿主删除的账号', () => {
  const app = appWithHistory()
  assert.deepEqual(app.historyAccountOptions.value, [
    { id: 'catalog-account', name: '目录名称', count: 1 },
    { id: 'deleted-account', name: '已删除账号', count: 2 },
  ])
})

test('清除历史使用打开时的账号、版本和数量快照，成功更新历史并关闭相同账号详情', async () => {
  const app = appWithHistory()
  requestCalls.length = 0
  const current = app.records.value[1]
  app.current.value = current
  app.historyDeleteAccountId.value = 'deleted-account'
  app.openHistoryDelete({ currentTarget: { isConnected: false } })

  assert.equal(app.historyDeleteExpectedVersion.value, 7)
  assert.equal(app.historyDeleteCount.value, 2)
  assert.equal(app.historyDeleteOpen.value, true)
  // 弹窗打开后选项变化不能改变已经确认的删除目标和名称。
  app.historyDeleteAccountId.value = 'catalog-account'
  assert.equal(app.historyDeleteName.value, '已删除账号')

  let payload
  nextResponse = { deleted: 2, history: { records: [app.records.value[0]], evicted: 3, version: 8 } }
  await app.deleteHistory()
  payload = requestCalls[0]
  assert.deepEqual(payload, { path: 'history/delete', body: { account_id: 'deleted-account', expected_version: 7 } })
  assert.equal(app.records.value.length, 1)
  assert.equal(app.evicted.value, 3)
  assert.equal(app.historyVersion.value, 8)
  assert.equal(app.current.value, null)
  assert.equal(app.historyDeleteOpen.value, false)
  assert.equal(app.notice.value, '已清除 2 条历史记录')
})

test('历史CAS冲突刷新后必须重新确认，不会静默重试删除', async () => {
  const app = appWithHistory()
  requestCalls.length = 0
  app.historyDeleteAccountId.value = 'deleted-account'
  app.openHistoryDelete({ currentTarget: { isConnected: false } })
  const conflict = new Error('测试记录已变化，请刷新并重新确认清除')
  conflict.status = 409
  nextResponse = conflict
  // 冲突后的刷新请求返回新版本；只调用一次删除请求，第二次调用前按钮保持不可用。
  let calls = 0
  api.request = async (path, body) => {
    requestCalls.push({ path, body })
    calls++
    if (path === 'history/delete') throw conflict
    return { records: [record('deleted-account', '已删除账号')], evicted: 0, version: 9 }
  }
  await app.deleteHistory()
  assert.equal(calls, 2)
  assert.equal(app.historyVersion.value, 9)
  assert.equal(app.historyDeleteStale.value, true)
  assert.match(app.historyDeleteError.value, /请关闭后重新选择并确认清除/)

  assert.equal(app.historyDeleteOpen.value, true)
})

test('显式transport才展示网络路径，旧探针记录未提供时不猜测', async () => {
  const direct = await renderToString(vue.createSSRApp({ render: () => vue.h(ProbeDetails, { metrics: { transport: 'direct' } }) }))
  const proxy = await renderToString(vue.createSSRApp({ render: () => vue.h(ProbeDetails, { metrics: { transport: 'account_proxy' } }) }))
  const legacy = await renderToString(vue.createSSRApp({ render: () => vue.h(ProbeDetails, { metrics: {} }) }))
  assert.match(direct, /网络路径：服务器直连/)
  assert.match(proxy, /网络路径：账号代理/)
  assert.doesNotMatch(legacy, /网络路径：服务器直连|网络路径：账号代理/)
})
