<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { request, type Account, type Key } from './api'
interface Settings { scheduled: boolean; auto_disable: boolean; interval_minutes: number; model: string; client_key_id: string | null; account_ids: string[] }
interface Status { next_due_ms: number; streak: number; last_status: string; last_checked_ms: number | null; action: string; action_detail: string }
interface Snapshot { state: { generation: string; settings: Settings; accounts: Record<string, Status>; last_tick_ms: number | null; last_error: string | null }; version: number | null }
const props = defineProps<{ accounts: Account[]; keys: Key[] }>()
const settings = ref<Settings>({ scheduled: false, auto_disable: false, interval_minutes: 30, model: 'gpt-6-astra', client_key_id: null, account_ids: [] })
const snapshot = ref<Snapshot | null>(null), busy = ref(false), error = ref(''), notice = ref(''), filter = ref('')
const candidates = computed(() => props.accounts.filter(a => a.provider === 'openai' && a.authentication_kind === 'oauth' && `${a.name} ${a.email || ''} ${a.id}`.toLowerCase().includes(filter.value.toLowerCase())))
function time(value: number | null) { return value ? new Date(value).toLocaleString('zh-CN', { hour12: false }) : '尚未执行' }
function name(id: string) { return props.accounts.find(a => a.id === id)?.name || id }
function statusLabel(status: string) { return ({ running: '结果未确认', skipped: '未执行', healthy: '未发现降级', degraded: '疑似降级', inconclusive: '无法判断' } as Record<string, string>)[status] || '尚未执行' }
function actionLabel(action: string) { return ({ pending: '停用待确认', disabled: '已自动停用', unconfirmed: '停用未确认' } as Record<string, string>)[action] || '无停用动作' }
async function load() {
  busy.value = true; error.value = ''; notice.value = ''
  try { snapshot.value = await request<Snapshot>('monitor'); settings.value = { ...snapshot.value.state.settings, account_ids: [...snapshot.value.state.settings.account_ids] } }
  catch (e) { error.value = (e as Error).message } finally { busy.value = false }
}
async function save() {
  busy.value = true; error.value = ''; notice.value = ''
  try {
    snapshot.value = await request<Snapshot>('monitor', { settings: settings.value, expected_generation: snapshot.value?.state.generation || null })
    settings.value = { ...snapshot.value.state.settings, account_ids: [...snapshot.value.state.settings.account_ids] }
    notice.value = '监控设置已保存；首轮按所设间隔执行，计数已重新开始'
  } catch (e) { error.value = (e as Error).message } finally { busy.value = false }
}
onMounted(load)
</script>

<template>
  <section class="panel">
    <div class="section-head"><strong>定时探针与自动停用</strong><div class="inline"><button :disabled="busy" @click="load">刷新监控状态</button><button class="primary" :disabled="busy || !snapshot" @click="save">保存监控设置</button></div></div>
    <div v-if="error" role="alert" class="alert error">{{ error }}</div>
    <div v-if="notice" role="status" class="alert success">{{ notice }}</div>
    <div class="alert warning">自动停用仅在连续两轮探针均为“疑似降级”时执行。超时、限流、认证失败或无法判断不触发停用，且中断连续计数。账号不会自动恢复。</div>
    <fieldset :disabled="busy">
      <div class="inline monitor-toggles"><label class="choice"><input v-model="settings.scheduled" type="checkbox">启用定时探针</label><label class="choice"><input v-model="settings.auto_disable" type="checkbox">连续两轮疑似降级后自动停用账号</label></div>
      <div class="form-grid"><label>监控客户端Key<select v-model="settings.client_key_id" aria-label="监控客户端Key"><option :value="null">请选择</option><option v-for="key in props.keys.filter(k => k.enabled)" :key="key.id" :value="key.id">{{ key.name || key.id }}</option></select></label><label>测试间隔（分钟）<input v-model.number="settings.interval_minutes" type="number" min="5" max="1440" step="1"></label><label>监控模型<input v-model="settings.model" maxlength="128"></label></div>
      <p class="muted">开关相互独立：自动停用也适用于所选账号、同一Key及模型的手动探针。题目测试不参与停用判定。</p>
      <div class="selection-header"><strong>监控账号 <span class="muted">已选 {{ settings.account_ids.length }} / 50</span></strong><div class="inline"><input v-model="filter" aria-label="筛选监控账号" placeholder="搜索名称、邮箱"><button type="button" @click="settings.account_ids = candidates.filter(a => a.enabled && a.proxy_configured).slice(0, 50).map(a => a.id)">选择筛选结果</button><button type="button" @click="settings.account_ids = []">取消选择</button></div></div>
      <div class="account-list"><label v-for="account in candidates" :key="account.id" class="choice"><input v-model="settings.account_ids" type="checkbox" :value="account.id" :disabled="(!account.enabled || !account.proxy_configured) && !settings.account_ids.includes(account.id)"><span>{{ account.name }}<small>{{ account.email && account.email !== account.name ? account.email + ' · ' : '' }}{{ account.enabled ? (account.proxy_configured ? 'OpenAI OAuth · 账号代理已配置' : '未配置账号代理') : '已停用，将跳过' }}</small></span></label><p v-if="!candidates.length" class="empty">暂无匹配的OpenAI OAuth账号</p></div>
    </fieldset>
    <details class="question-preview"><summary>后台执行与安全边界</summary><div class="help">关闭页面后，宿主维护任务仍继续执行；停用插件后停止。每次维护最多测试一个到期账号，多账号依到期顺序执行，繁忙时顺延，不并行重试。定时探针最多等待18秒，手动探针最多90秒。修改并保存计划会重置计数及到期时间。停用通过宿主管理接口仅更新账号启用状态；未确认动作不会自动重发，请在账号管理中核对。仅人工恢复账号后才会重新监控，不自动启用任何账号。探针按Key限定账号和模型范围，强制走账号代理；未配置或代理失败不直连。探针不计入Key账单，不自动刷新令牌；只在本次调用内存读取所选单账号的敏感导出以取得完整代理配置，敏感内容不返回页面、不写状态或日志。</div></details>
  </section>
  <section v-if="snapshot" class="panel">
    <div class="section-head"><strong>监控状态</strong><span class="muted">最近后台检查 {{ time(snapshot.state.last_tick_ms) }}</span></div>
    <div v-if="snapshot.state.last_error" class="alert warning">{{ snapshot.state.last_error }}</div>
    <div v-if="Object.keys(snapshot.state.accounts).length" class="table-wrap"><table><thead><tr><th>账号</th><th>最近测试</th><th>结果</th><th>连续命中</th><th>下次到期</th><th>账号处置</th></tr></thead><tbody><tr v-for="(state, id) in snapshot.state.accounts" :key="id"><td>{{ name(id) }}</td><td>{{ time(state.last_checked_ms) }}</td><td>{{ statusLabel(state.last_status) }}</td><td>{{ state.streak }} / 2</td><td>{{ snapshot.state.settings.scheduled ? time(state.next_due_ms) : '计划未启用' }}</td><td>{{ actionLabel(state.action) }}<small>{{ state.action_detail }}</small></td></tr></tbody></table></div>
    <p v-else class="empty">尚未配置监控账号</p>
  </section>
</template>
