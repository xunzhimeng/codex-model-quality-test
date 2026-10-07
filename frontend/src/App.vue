<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from 'vue'
import { request, type Account, type Catalog, type History, type Question, type TestRecord } from './api'
import { csvCell, runAccountQueue } from './queue'
const catalog = ref<Catalog>({ accounts: [], keys: [], questions: [], bank_version: null, version: '' })
const records = ref<TestRecord[]>([]), evicted = ref(0), error = ref(''), notice = ref(''), loading = ref(false)
const tab = ref('new'), mode = ref('question'), accountFilter = ref(''), selectedAccounts = ref<string[]>([]), selectedQuestions = ref<string[]>([])
const key = ref(''), model = ref(''), effort = ref('medium'), concurrency = ref(2), models = ref<string[]>([]), modelsLoading = ref(false)
const running = ref(false), stopping = ref(false), completed = ref(0), total = ref(0), batchId = ref(''), current = ref<TestRecord | null>(null)
const customJson = ref(''), bankSaving = ref(false)
const dialog = ref<HTMLElement | null>(null)
let previousFocus: HTMLElement | null = null
watch(current, async (value) => {
  if (value) { previousFocus = document.activeElement as HTMLElement; await nextTick(); dialog.value?.querySelector<HTMLButtonElement>('button')?.focus() }
  else previousFocus?.focus()
})
function trapFocus(event: KeyboardEvent) {
  if (event.key !== 'Tab' || !dialog.value) return
  const nodes = [...dialog.value.querySelectorAll<HTMLElement>('button, [href], input, textarea, select, [tabindex="0"]')]
  const first = nodes[0], last = nodes[nodes.length - 1]
  if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus() }
  else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus() }
}
const visibleAccounts = computed(() => catalog.value.accounts.filter(a => !accountFilter.value || `${a.name} ${a.id} ${a.provider}`.toLowerCase().includes(accountFilter.value.toLowerCase())))
const displayedRecords = computed(() => tab.value === 'new' ? records.value.filter(r => r.batch_id === batchId.value) : records.value)
const selectedQuestion = computed(() => catalog.value.questions.find(q => q.id === selectedQuestions.value[0]))
const comparison = computed(() => {
  const map = new Map<string, { id: string; name: string; correct: number; wrong: number; failed: number; pending: number; latency: number; count: number; probe: string }>()
  for (const r of displayedRecords.value) {
    let row = map.get(r.account_id)
    if (!row) { row = { id: r.account_id, name: r.account_name || r.account_id, correct: 0, wrong: 0, failed: 0, pending: 0, latency: 0, count: 0, probe: '—' }; map.set(r.account_id, row) }
    if (r.mode === 'probe') { if (row.probe === '—') row.probe = label(r.status) }
    else { if (r.status === 'correct') row.correct++; if (r.status === 'wrong') row.wrong++; if (r.status === 'failed') row.failed++; if (r.status === 'running') row.pending++ }
    if (r.latency_ms !== null) { row.latency += r.latency_ms; row.count++ }
  }
  return Array.from(map.values())
})
function label(status: string) { return ({ correct: '答对', wrong: '答错', failed: '调用失败', running: '结果未确认', healthy: '未发现降级', degraded: '疑似降级', inconclusive: '无法判断' } as Record<string, string>)[status] || status }
function seconds(ms: number | null) { return ms === null ? '—' : `${(ms / 1000).toFixed(2)} s` }
function time(ms: number) { return new Date(ms).toLocaleString('zh-CN', { hour12: false }) }
function upsert(record: TestRecord) { const i = records.value.findIndex(r => r.id === record.id); if (i === -1) records.value.unshift(record); else records.value[i] = record }
async function refreshHistory() { const history = await request<History>('/history'); records.value = [...history.records].reverse(); evicted.value = history.evicted }
async function reload() {
  error.value = ''; loading.value = true
  try {
    catalog.value = await request<Catalog>('/catalog')
    customJson.value = JSON.stringify(catalog.value.questions.filter(q => q.id.startsWith('custom_')), null, 2)
    selectedAccounts.value = selectedAccounts.value.filter(id => catalog.value.accounts.some(a => a.id === id))
    if (!selectedQuestions.value.length) selectedQuestions.value = catalog.value.questions.map(q => q.id)
    else selectedQuestions.value = selectedQuestions.value.filter(id => catalog.value.questions.some(q => q.id === id))
    if (!catalog.value.keys.some(k => k.id === key.value && k.enabled)) key.value = catalog.value.keys.find(k => k.enabled)?.id || ''
    await refreshHistory(); await loadModels()
  } catch (e) { error.value = (e as Error).message } finally { loading.value = false }
}
let modelLoad = 0
async function loadModels() {
  const version = ++modelLoad; models.value = []
  if (!key.value) return
  modelsLoading.value = true
  try {
    const result = await request<{ models: string[] }>('/models', { client_key_id: key.value })
    if (version !== modelLoad) return
    models.value = result.models
    if (!models.value.includes(model.value)) model.value = models.value.includes('gpt-6-astra') ? 'gpt-6-astra' : models.value[0] || ''
  } catch (e) { if (version === modelLoad) error.value = (e as Error).message } finally { if (version === modelLoad) modelsLoading.value = false }
}
function changeMode() { if (mode.value === 'probe' && !model.value) model.value = 'gpt-6-astra' }
function selectAccounts() { selectedAccounts.value = visibleAccounts.value.filter(a => a.enabled && (mode.value !== 'probe' || a.probe_supported)).map(a => a.id) }
async function run() {
  error.value = ''; notice.value = ''
  if (!selectedAccounts.value.length || !model.value.trim()) { error.value = '请选择账号与模型'; return }
  if (mode.value === 'question' && (!key.value || !selectedQuestions.value.length || !models.value.includes(model.value))) { error.value = '请选择可见模型、客户端Key与题目'; return }
  const accounts = catalog.value.accounts.filter(a => selectedAccounts.value.includes(a.id))
  if (accounts.some(a => !a.enabled || (mode.value === 'probe' && !a.probe_supported))) { error.value = '选中账号不支持当前测试，请重新选择'; return }
  const tasks = mode.value === 'probe' ? [null] : [...selectedQuestions.value]
  const settings = { mode: mode.value, model: model.value.trim(), effort: mode.value === 'probe' ? 'default' : effort.value, client_key_id: mode.value === 'probe' ? null : key.value }
  batchId.value = crypto.randomUUID(); const id = batchId.value; total.value = accounts.length * tasks.length; completed.value = 0
  running.value = true; stopping.value = false
  try {
    await runAccountQueue(accounts, tasks, Math.max(1, Math.min(4, concurrency.value)), async (account: Account, question: string | null) => {
      const operation = crypto.randomUUID()
      try {
        const result = await request<{ record: TestRecord }>('/run', { id: operation, batch_id: id, account_id: account.id, ...settings, question_id: question })
        upsert(result.record)
        if (result.record.status === 'running') { stopping.value = true; error.value = '测试结果未确认，请刷新历史后再操作' }
      } catch (e) {
        stopping.value = true; error.value = `${(e as Error).message}，操作ID ${operation}，未自动重试`
      } finally { completed.value++ }
    }, () => stopping.value)
    notice.value = stopping.value ? '已停止后续任务，已发送的测试不会自动重试' : `本批次已完成 ${completed.value} 项测试`
    await refreshHistory()
  } catch (e) { error.value = (e as Error).message } finally { running.value = false }
}
async function saveBank() {
  bankSaving.value = true; error.value = ''; notice.value = ''
  try {
    const questions: Question[] = JSON.parse(customJson.value)
    if (!Array.isArray(questions)) throw new Error('题库须为JSON数组')
    await request('/questions', { questions, expected_version: catalog.value.bank_version })
    await reload(); notice.value = '自定义题库已保存'
  } catch (e) { error.value = (e as Error).message } finally { bankSaving.value = false }
}
function download(format: 'json' | 'csv') {
  const list = displayedRecords.value
  let content: string
  if (format === 'json') content = JSON.stringify({ exported_at: new Date().toISOString(), records: list }, null, 2)
  else {
    const headers = ['时间', '批次', '账号', '模式', '题目', '模型', '思考强度', '结果', '耗时毫秒', '回答', '标准答案', '详情']
    const rows = list.map(r => [time(r.started_at_ms), r.batch_id, r.account_name || r.account_id, r.mode, r.question_title, r.model, r.effort, label(r.status), r.latency_ms, r.answer, r.expected, r.detail])
    content = '\uFEFF' + [headers, ...rows].map(row => row.map(csvCell).join(',')).join('\r\n')
  }
  const url = URL.createObjectURL(new Blob([content], { type: format === 'json' ? 'application/json' : 'text/csv;charset=utf-8' }))
  const a = document.createElement('a'); a.href = url; a.download = `model-quality-${Date.now()}.${format}`; a.click(); setTimeout(() => URL.revokeObjectURL(url), 1000)
}
onMounted(reload)
</script>

<template>
  <main>
    <div class="toolbar">
      <nav class="tabs" aria-label="测试页面">
        <button v-for="item in [['new', '新建测试'], ['history', '测试记录'], ['bank', '自定义题库']]" :key="item[0]" :class="{ active: tab === item[0] }" :disabled="running" @click="tab = item[0]">{{ item[1] }}</button>
      </nav>
      <button :disabled="loading || running" @click="reload">{{ loading ? '正在加载' : '刷新' }}</button>
    </div>
    <div v-if="error" class="alert error" role="alert">{{ error }}</div>
    <div v-if="notice" class="alert success" role="status">{{ notice }}</div>
    <template v-if="tab === 'new'">
      <section class="panel">
        <div class="section-head"><nav class="segments" aria-label="测试类型"><button :class="{ active: mode === 'question' }" :disabled="running" @click="mode = 'question'">题目测试</button><button :class="{ active: mode === 'probe' }" :disabled="running" @click="mode = 'probe'; changeMode()">探针测试</button></nav>
          <details><summary>测试说明</summary><div class="help"><template v-if="mode === 'probe'">仅适用于OpenAI OAuth<br>首轮门票加路由Cookie续接，返回不同门票标记为疑似降级<br>经验判据不是官方模型质量证明，响应模型名称不用于判定</template><template v-else>严格比较标准答案，仅忽略首尾空白和包裹整个答案的Markdown标记<br>调用失败不计为答错，单次答错不等于账号降级<br>题目测试通过宿主Key权限、租约与计费链</template></div></details>
        </div>
        <div v-if="mode === 'probe'" class="alert warning">探针不走Key计费链，不继承账号专属代理，不自动刷新令牌</div>
        <fieldset :disabled="running || loading">
          <div class="form-grid">
            <label v-if="mode === 'question'">客户端Key<select v-model="key" @change="loadModels"><option value="">请选择</option><option v-for="k in catalog.keys.filter(k => k.enabled)" :key="k.id" :value="k.id">{{ k.name || k.id }}</option></select></label>
            <label>模型<input v-model="model" list="model-options" maxlength="128" :placeholder="modelsLoading ? '正在加载可见模型' : '请选择或输入模型'"><datalist id="model-options"><option v-for="name in models" :key="name" :value="name" /></datalist></label>
            <label v-if="mode === 'question'">思考强度<select v-model="effort"><option value="default">默认</option><option value="low">低</option><option value="medium">中</option><option value="high">高</option><option value="xhigh">极高</option><option value="max">最高</option></select></label>
            <label>账号并发数<select v-model="concurrency"><option v-for="n in 4" :key="n" :value="n">{{ n }}</option></select></label>
          </div>
          <div class="selection-header"><strong>账号 <span class="muted">已选 {{ selectedAccounts.length }}</span></strong><div class="inline"><input v-model="accountFilter" aria-label="筛选账号" placeholder="搜索账号"><button type="button" @click="selectAccounts">选择筛选结果</button><button type="button" @click="selectedAccounts = []">取消选择</button></div></div>
          <div class="account-list"><label v-for="account in visibleAccounts" :key="account.id" class="choice"><input v-model="selectedAccounts" type="checkbox" :value="account.id" :disabled="!account.enabled || (mode === 'probe' && !account.probe_supported)"><span>{{ account.name || account.id }}<small>{{ account.provider }} · {{ account.authentication_kind }}{{ !account.enabled ? ' · 已停用' : mode === 'probe' && !account.probe_supported ? ' · 不适用或令牌过期' : '' }}</small></span></label><p v-if="!visibleAccounts.length" class="empty">暂无匹配账号</p></div>
          <template v-if="mode === 'question'">
            <div class="selection-header"><strong>测试题目 <span class="muted">已选 {{ selectedQuestions.length }}</span></strong><div class="inline"><button type="button" @click="selectedQuestions = catalog.questions.map(q => q.id)">全选</button><button type="button" @click="selectedQuestions = []">取消选择</button></div></div>
            <div class="question-list"><label v-for="q in catalog.questions" :key="q.id" class="choice"><input v-model="selectedQuestions" type="checkbox" :value="q.id"><span>{{ q.title }}<small>{{ q.category }}</small></span></label></div>
            <details v-if="selectedQuestion" class="question-preview"><summary>查看首道所选题目</summary><pre>{{ selectedQuestion.prompt }}</pre><span class="muted">标准答案 {{ selectedQuestion.answer }}</span></details>
          </template>
        </fieldset>
        <div class="runbar"><span class="muted">{{ running ? `已完成 ${completed} / ${total}` : '同一账号按题目顺序执行，关闭页面后不再发送后续任务' }}</span><button v-if="running" :disabled="stopping" @click="stopping = true">{{ stopping ? '等待已发送任务结束' : '停止后续任务' }}</button><button v-else class="primary" :disabled="loading || !selectedAccounts.length || !model || (mode === 'question' && (!key || modelsLoading || !selectedQuestions.length))" @click="run">{{ mode === 'probe' ? '开始探针' : '开始测试' }}</button></div>
      </section>
    </template>
    <section v-if="tab === 'bank'" class="panel">
      <div class="section-head"><strong>自定义题库</strong><button class="primary" :disabled="bankSaving" @click="saveBank">{{ bankSaving ? '正在保存' : '保存题库' }}</button></div>
      <p class="muted">最多30道，每题包含 id、title、category、prompt、answer，ID以 custom_ 开头</p>
      <details><summary>查看格式示例</summary><pre>[{"id":"custom_sum","title":"加法","category":"数学","prompt":"17加28等于多少？只输出整数","answer":"45"}]</pre></details>
      <label class="editor-label">题库JSON<textarea v-model="customJson" spellcheck="false" rows="16" /></label>
    </section>
    <template v-else>
      <section v-if="comparison.length" class="panel">
        <div class="section-head"><strong>账号对比</strong><span class="muted">正确率仅统计已评分题目，历史页汇总当前保留记录</span></div>
        <div class="table-wrap"><table><thead><tr><th>账号</th><th>答对 / 已评分</th><th>正确率</th><th>调用失败</th><th>未确认</th><th>平均耗时</th><th>最近探针</th></tr></thead><tbody><tr v-for="row in comparison" :key="row.id"><td>{{ row.name }}</td><td>{{ row.correct }} / {{ row.correct + row.wrong }}</td><td>{{ row.correct + row.wrong ? `${Math.round(row.correct / (row.correct + row.wrong) * 100)}%` : '—' }}</td><td>{{ row.failed }}</td><td>{{ row.pending }}</td><td>{{ row.count ? seconds(row.latency / row.count) : '—' }}</td><td>{{ row.probe }}</td></tr></tbody></table></div>
      </section>
      <section class="panel">
        <div class="section-head"><strong>{{ tab === 'history' ? '测试记录' : '本批次结果' }}</strong><div class="inline"><button :disabled="!displayedRecords.length" @click="download('json')">导出JSON</button><button :disabled="!displayedRecords.length" @click="download('csv')">导出CSV</button></div></div>
        <p v-if="tab === 'history'" class="muted">最多保留100项，并受195KB容量限制{{ evicted ? `，累计移出 ${evicted} 项，请及时导出` : '' }}</p>
        <div v-if="displayedRecords.length" class="table-wrap"><table><thead><tr><th>时间</th><th>账号</th><th>题目 / 探针</th><th>模型</th><th>结果</th><th>耗时</th><th>操作</th></tr></thead><tbody><tr v-for="r in displayedRecords" :key="r.id"><td class="nowrap">{{ time(r.started_at_ms) }}</td><td>{{ r.account_name || r.account_id }}</td><td>{{ r.question_title || '门票探针' }}</td><td>{{ r.model }}<small v-if="r.mode === 'question'">{{ r.effort }}</small></td><td><span class="badge" :class="r.status">{{ label(r.status) }}</span></td><td class="nowrap">{{ seconds(r.latency_ms) }}</td><td><button class="text-button" @click="current = r">详情</button></td></tr></tbody></table></div>
        <p v-else class="empty">{{ loading ? '正在加载' : '暂无测试结果' }}</p>
      </section>
    </template>
    <div v-if="current" class="overlay" @click.self="current = null"><section ref="dialog" class="dialog" role="dialog" aria-modal="true" aria-labelledby="detail-title" @keydown="trapFocus" @keydown.esc="current = null"><div class="section-head"><strong id="detail-title">测试详情</strong><button autofocus @click="current = null">关闭</button></div><p><span class="badge" :class="current.status">{{ label(current.status) }}</span> {{ current.detail }}</p><p class="muted">操作ID {{ current.id }}</p><template v-if="current.prompt"><h4>测试题目</h4><pre>{{ current.prompt }}</pre><h4>模型回答</h4><pre>{{ current.answer || '暂无完整回答' }}</pre><p>标准答案 {{ current.expected }}</p></template><h4>执行指标</h4><pre>{{ JSON.stringify(current.metrics, null, 2) }}</pre></section></div>
  </main>
</template>
