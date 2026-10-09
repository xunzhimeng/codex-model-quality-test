<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { request, type Account, type Key } from './api'
import { useModels } from './models'
interface Settings { scheduled: boolean; auto_disable: boolean; interval_minutes: number; model: string; client_key_id: string | null; account_ids: string[]; daily_max: number; quiet_hours: { start: string; end: string } | null; degradation_policy: 'confirm' | 'immediate'; confirmation_delay_seconds: number }
interface Status { next_due_ms: number; streak: number; last_status: string; last_checked_ms: number | null; action: string; action_detail: string; paused: boolean; total_runs: number; daily_runs: number; confirm_due_ms: number | null }
interface Snapshot { state: { generation: string; settings: Settings; accounts: Record<string, Status>; last_tick_ms: number | null; last_error: string | null; paused: boolean; control_revision: number }; availability: Record<string, { status: string; next_allowed_ms: number | null }>; total_runs: number; daily_runs: number; version: number | null }
const props = defineProps<{ accounts: Account[]; keys: Key[] }>()
const settings = ref<Settings>({ scheduled: false, auto_disable: false, interval_minutes: 30, model: '', client_key_id: null, account_ids: [], daily_max: 48, quiet_hours: null, degradation_policy: 'confirm', confirmation_delay_seconds: 60 })
const quietEnabled = computed({ get: () => !!settings.value.quiet_hours, set: enabled => { settings.value.quiet_hours = enabled ? { start: '23:00', end: '07:00' } : null } })
const { models, modelsLoading, modelsError, loadModels } = useModels(() => settings.value.client_key_id, computed({ get: () => settings.value.model, set: value => { settings.value.model = value } }))
const snapshot = ref<Snapshot | null>(null), editGeneration = ref(''), busy = ref(false), error = ref(''), notice = ref(''), filter = ref('')
const candidates = computed(() => props.accounts.filter(a => a.provider === 'openai' && a.authentication_kind === 'oauth' && `${a.name} ${a.email || ''} ${a.id}`.toLowerCase().includes(filter.value.toLowerCase())))
const rows = computed(() => snapshot.value?.state.settings.account_ids || [])
function time(value: number | null | undefined) { return value ? new Date(value).toLocaleString('zh-CN', { hour12: false, timeZone: 'Asia/Shanghai' }) : '—' }
function name(id: string) { return props.accounts.find(a => a.id === id)?.name || id }
function statusLabel(status: string) { return ({ running: '执行中或结果未确认', skipped: '未执行', interrupted: '执行中断', healthy: '未发现降级', degraded: '疑似降级', inconclusive: '无法判断' } as Record<string, string>)[status] || '尚未执行' }
function actionLabel(action: string) { return ({ pending: '停用待确认', disabled: '已自动停用', unconfirmed: '停用未确认' } as Record<string, string>)[action] || '无停用动作' }
function adopt(result: Snapshot) { editGeneration.value = result.state.generation; settings.value = { ...result.state.settings, account_ids: [...result.state.settings.account_ids], quiet_hours: result.state.settings.quiet_hours ? { ...result.state.settings.quiet_hours } : null } }
async function load() {
  busy.value = true; error.value = ''; notice.value = ''
  try { const initial = !snapshot.value; snapshot.value = await request<Snapshot>('monitor'); if (initial) { adopt(snapshot.value); await loadModels() } }
  catch (e) { error.value = (e as Error).message } finally { busy.value = false }
}
async function reloadSettings() { if (!snapshot.value) return; busy.value = true; adopt(snapshot.value); await loadModels(); busy.value = false }
async function save() {
  if (!Number.isInteger(settings.value.daily_max) || settings.value.daily_max < 1 || settings.value.daily_max > 1440) { error.value = '每日每账号上限须为1至1440的整数'; return }
  if (!Number.isInteger(settings.value.interval_minutes) || settings.value.interval_minutes < 5 || settings.value.interval_minutes > 1440) { error.value = '测试间隔须为5至1440的整数分钟'; return }
  if (!Number.isInteger(settings.value.confirmation_delay_seconds) || settings.value.confirmation_delay_seconds < 1 || settings.value.confirmation_delay_seconds > 86400) { error.value = '复测等待须为1至86400的整数秒'; return }
  if (modelsLoading.value) { error.value = '模型目录正在加载，请稍后保存'; return }
  busy.value = true; error.value = ''; notice.value = ''
  try {
    snapshot.value = await request<Snapshot>('monitor', { settings: settings.value, expected_generation: editGeneration.value || null })
    adopt(snapshot.value)
    notice.value = !settings.value.scheduled ? '定时探针与自动停用已关闭，已发出的操作不撤回' : snapshot.value.state.paused ? '监控设置已保存，整组仍处于暂停状态；确认命中已清零，执行次数保留' : '监控设置已保存，首轮按所设间隔执行；确认命中已清零，执行次数保留'
  } catch (e) { error.value = (e as Error).message } finally { busy.value = false }
}
async function control(paused: boolean, accountId: string | null = null) {
  if (!snapshot.value) return
  busy.value = true; error.value = ''; notice.value = ''
  try {
    snapshot.value = await request<Snapshot>('monitor/control', { paused, account_id: accountId, expected_generation: snapshot.value.state.generation, expected_control_revision: snapshot.value.state.control_revision })
    notice.value = paused ? '监控已暂停，已发出的操作不撤回，确认命中已清零' : snapshot.value.state.paused ? '该账号已继续，但整组仍处于暂停状态' : '监控已继续，下次按常规间隔执行，仍遵守不测试时段及每日上限'
  } catch (e) { error.value = (e as Error).message } finally { busy.value = false }
}
onMounted(load)
</script>

<template>
  <section class="panel">
    <div class="section-head"><strong>定时探针与自动停用</strong><div class="inline"><button :disabled="busy" @click="load">刷新监控状态</button><button class="primary" :disabled="busy || !snapshot" @click="save">保存监控设置</button></div></div>
    <div v-if="snapshot && snapshot.state.generation !== editGeneration" class="alert warning">已保存设置已变化，请先刷新状态并重载设置 <button :disabled="busy" @click="reloadSettings">重载设置</button></div>
    <div v-if="error" role="alert" class="alert error">{{ error }}</div>
    <div v-if="notice" role="status" class="alert success">{{ notice }}</div>
    <div class="alert warning">仅后台探针明确“疑似降级”可触发停用，须开启自动停用。{{ settings.degradation_policy === 'immediate' ? '立即停用策略不复测，误停风险较高。' : '按设定秒数等待后确认一次，仅复测仍疑似降级才停用。' }}超时、限流、认证失败或无法判断不触发停用。账号不会自动恢复。</div>
    <fieldset :disabled="busy">
      <div class="inline monitor-toggles"><label class="choice"><input v-model="settings.scheduled" type="checkbox" @change="!settings.scheduled && (settings.auto_disable = false)">启用定时探针</label><label class="choice"><input v-model="settings.auto_disable" type="checkbox" :disabled="!settings.scheduled">启用自动停用账号</label></div>
      <div class="form-grid"><label>监控客户端Key<select v-model="settings.client_key_id" aria-label="监控客户端Key" @change="loadModels"><option :value="null">请选择</option><option v-for="key in props.keys.filter(k => k.enabled)" :key="key.id" :value="key.id">{{ key.name || key.id }}</option></select></label><label>测试间隔（分钟）<input v-model.number="settings.interval_minutes" type="number" min="5" max="1440" step="1"></label><label>监控模型<select v-model="settings.model" aria-label="监控模型" :disabled="modelsLoading || !models.length"><option v-if="!models.length" :value="modelsError ? settings.model : ''">{{ modelsLoading ? '正在加载可见模型' : modelsError ? settings.model || '模型加载失败' : settings.client_key_id ? '暂无可见模型' : '请先选择Key' }}</option><option v-for="model in models" :key="model" :value="model">{{ model }}</option></select><small v-if="modelsError" role="alert">模型加载失败 {{ modelsError }} <button class="text-button" type="button" @click="loadModels">重试</button></small><small v-else-if="settings.client_key_id && !modelsLoading && !models.length">所选Key暂无可见模型，请在宿主检查模型范围</small></label></div>
      <div class="form-grid"><label>降级处理策略<select v-model="settings.degradation_policy" aria-label="降级处理策略"><option value="confirm">延迟复测，仍疑似降级时停用</option><option value="immediate">首次疑似降级立即停用，不复测</option></select></label><label v-if="settings.degradation_policy === 'confirm'">复测等待（秒）<input v-model.number="settings.confirmation_delay_seconds" aria-label="复测等待（秒）" type="number" min="1" max="86400" step="1"><small>到期后由宿主维护回调执行，实际等待可能更长</small></label></div>
      <div class="form-grid"><label>每日每账号最大测试次数<input v-model.number="settings.daily_max" type="number" aria-label="每日每账号最大测试次数" min="1" max="1440" step="1"><small>一次探针任务计1次，确认复测另计1次</small></label><label class="choice"><input v-model="quietEnabled" type="checkbox">启用不测试时段</label><div v-if="settings.quiet_hours" class="inline"><label>不测试开始<input v-model="settings.quiet_hours.start" type="time" aria-label="不测试开始"></label><label>不测试结束<input v-model="settings.quiet_hours.end" type="time" aria-label="不测试结束"></label></div></div>
      <p class="muted">按北京时间（UTC+8）每日零点重置，时段可跨午夜。时间与次数限制优先于复测；手动探针不受限制、不计数，也不触发自动停用。</p>
      <div class="selection-header"><strong>监控账号 <span class="muted">已选 {{ settings.account_ids.length }} / 50</span></strong><div class="inline"><input v-model="filter" aria-label="筛选监控账号" placeholder="搜索名称、邮箱"><button type="button" @click="settings.account_ids = candidates.filter(a => a.enabled && a.proxy_configured).slice(0, 50).map(a => a.id)">选择筛选结果</button><button type="button" @click="settings.account_ids = []">取消选择</button></div></div>
      <div class="account-list"><label v-for="account in candidates" :key="account.id" class="choice"><input v-model="settings.account_ids" type="checkbox" :value="account.id" :disabled="(!account.enabled || !account.proxy_configured) && !settings.account_ids.includes(account.id)"><span>{{ account.name }}<small>{{ account.email && account.email !== account.name ? account.email + ' · ' : '' }}{{ account.enabled ? (account.proxy_configured ? 'OpenAI OAuth · 账号代理已配置' : '未配置账号代理') : '已停用，将跳过' }}</small></span></label><p v-if="!candidates.length" class="empty">暂无匹配的OpenAI OAuth账号</p></div>
    </fieldset>
    <details class="question-preview"><summary>后台执行与安全边界</summary><div class="help">关闭页面后，宿主维护任务仍继续执行；停用插件后停止。每次维护最多测试一个到期账号，繁忙时顺延；预约确认由后续维护回调执行，等待不少于配置秒数，不保证精确到秒；确认等待期间不启动同账号的常规探针，确认到期后超过一个常规间隔或跨过不测试时段则作废，不以陈旧结果停用。暂停或保存会清空确认链，不撤回已发出的请求；继续后按常规间隔执行。执行次数自本版开始累计，按后台准入次数统计，已准入但失败或取消仍计次；保存、暂停及进程重启不清空次数。每日上限包含确认复测，达到上限后次日恢复；不测试时段不补发堆积任务。手动探针和题目测试不参与监控判定。定时探针最多等待18秒，手动探针最多90秒。停用动作未确认时不自动重发，需在账号管理核对；仅人工恢复账号后重新监控。探针按Key限定账号与模型范围，强制走账号代理；失败不直连，不计入Key账单，不自动刷新令牌。敏感导出仅限所选单账号和本次调用内存，敏感内容不返回页面、不写状态或日志。</div></details>
  </section>
  <section v-if="snapshot" class="panel">
    <div class="section-head"><strong>监控状态</strong><div class="inline"><span class="muted">{{ snapshot.state.paused ? '整组已暂停' : snapshot.state.settings.scheduled ? '整组运行中' : '计划未启用' }}</span><button :disabled="busy || !snapshot.state.settings.scheduled" @click="control(!snapshot.state.paused)">{{ snapshot.state.paused ? '继续全部' : '暂停全部' }}</button></div></div>
    <p class="muted">累计执行 {{ snapshot.total_runs }} 次 · 今日执行 {{ snapshot.daily_runs }} 次 · 北京时间 · 最近后台检查 {{ time(snapshot.state.last_tick_ms) }}（含曾监控账号）</p>
    <div v-if="snapshot.state.last_error" class="alert warning">{{ snapshot.state.last_error }}</div>
    <div v-if="rows.length" class="table-wrap"><table><thead><tr><th>账号</th><th>监控状态</th><th>累计 / 今日次数</th><th>最近测试</th><th>结果</th><th>降级命中</th><th>下次可执行</th><th>账号处置</th><th>操作</th></tr></thead><tbody><tr v-for="id in rows" :key="id"><td>{{ name(id) }}</td><td>{{ snapshot.availability[id]?.status }}</td><td>{{ snapshot.state.accounts[id]?.total_runs || 0 }} / {{ snapshot.state.accounts[id]?.daily_runs || 0 }}<small>今日上限 {{ snapshot.state.settings.daily_max }}</small></td><td>{{ time(snapshot.state.accounts[id]?.last_checked_ms) }}</td><td>{{ statusLabel(snapshot.state.accounts[id]?.last_status || '') }}</td><td>{{ snapshot.state.accounts[id]?.streak || 0 }} / {{ snapshot.state.settings.degradation_policy === 'immediate' ? 1 : 2 }}</td><td>{{ time(snapshot.availability[id]?.next_allowed_ms) }}</td><td>{{ actionLabel(snapshot.state.accounts[id]?.action || '') }}<small>{{ snapshot.state.accounts[id]?.action_detail }}</small></td><td><button :disabled="busy || !snapshot.state.settings.scheduled" :aria-label="(snapshot.state.accounts[id]?.paused ? '继续监控 ' : '暂停监控 ') + name(id)" @click="control(!snapshot.state.accounts[id]?.paused, id)">{{ snapshot.state.accounts[id]?.paused ? '继续' : '暂停' }}</button></td></tr></tbody></table></div>
    <p v-else class="empty">尚未配置监控账号</p>
  </section>
</template>
