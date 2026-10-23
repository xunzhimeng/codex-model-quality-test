<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from 'vue'
import { request, type Account, type Key } from './api'
import { useModels } from './models'
interface Settings { scheduled: boolean; auto_disable: boolean; interval_minutes: number; model: string; client_key_id: string | null; account_ids: string[]; daily_max: number; quiet_hours: { start: string; end: string } | null; degradation_policy: 'confirm' | 'immediate'; confirmation_delay_seconds: number }
interface Status { next_due_ms: number; streak: number; last_status: string; last_checked_ms: number | null; action: string; action_detail: string; paused: boolean; total_runs: number; daily_runs: number; confirm_due_ms: number | null }
interface State { generation: string; settings: Settings; accounts: Record<string, Status>; last_tick_ms: number | null; last_error: string | null; paused: boolean; control_revision: number }
interface Task { id: string; name: string; state: State; availability: Record<string, { status: string; next_allowed_ms: number | null }>; total_runs: number; daily_runs: number }
interface Snapshot { tasks: Task[]; defaults: Settings; total_runs: number; daily_runs: number; timezone: string; now_ms: number; control_revision: number }
const props = defineProps<{ accounts: Account[]; keys: Key[] }>()
const snapshot = ref<Snapshot | null>(null), busy = ref(false), error = ref(''), notice = ref('')
const dialog = ref<HTMLDialogElement | null>(null), refreshButton = ref<HTMLButtonElement | null>(null), editorOpen = ref(false), editingId = ref<string | null>(null), editGeneration = ref<string | null>(null)
const settings = ref<Settings | null>(null), taskName = ref(''), filter = ref(''), editorError = ref('')
let previousFocus: HTMLElement | null = null
const quietEnabled = computed({ get: () => !!settings.value?.quiet_hours, set: enabled => { if (settings.value) settings.value.quiet_hours = enabled ? { start: '23:00', end: '07:00' } : null } })
const { models, modelsLoading, modelsError, loadModels } = useModels(() => settings.value?.client_key_id || null, computed({ get: () => settings.value?.model || '', set: value => { if (settings.value) settings.value.model = value } }))
const candidates = computed(() => props.accounts.filter(a => a.provider === 'openai' && a.authentication_kind === 'oauth' && `${a.name} ${a.email || ''} ${a.id}`.toLowerCase().includes(filter.value.toLowerCase())))
const editedTask = computed(() => snapshot.value?.tasks.find(task => task.id === editingId.value))
const staleEditor = computed(() => editingId.value !== null && (!editedTask.value || editedTask.value.state.generation !== editGeneration.value))
function copySettings(value: Settings): Settings { return { ...value, account_ids: [...value.account_ids], quiet_hours: value.quiet_hours ? { ...value.quiet_hours } : null } }
function time(value: number | null | undefined) { return value ? new Date(value).toLocaleString('zh-CN', { hour12: false, timeZone: snapshot.value?.timezone || 'Asia/Shanghai' }) : '—' }
function name(id: string) { return props.accounts.find(a => a.id === id)?.name || id }
function statusLabel(status: string) { return ({ running: '执行中或结果未确认', skipped: '未执行', interrupted: '执行中断', healthy: '未发现降级', degraded: '疑似降级', inconclusive: '无法判断' } as Record<string, string>)[status] || '尚未执行' }
function actionLabel(action: string) { return ({ pending: '停用待确认', disabled: '已自动停用', unconfirmed: '停用未确认' } as Record<string, string>)[action] || '无停用动作' }
function taskStatus(task: Task) { return !task.state.settings.scheduled ? '未启用' : task.state.paused ? '已暂停' : '运行中' }
function failure(e: unknown) {
  const message = (e as Error).message
  return /变化|冲突|代次|版本|conflict|generation|revision/i.test(message) ? `${message}，请刷新状态并重载任务后重试` : message
}
async function load() {
  if (busy.value) return
  busy.value = true; error.value = ''; notice.value = ''
  try { snapshot.value = await request<Snapshot>('monitor') }
  catch (e) { error.value = failure(e) } finally { busy.value = false }
}
function adopt(task: Task | null) {
  if (!snapshot.value) return
  editingId.value = task?.id || null; editGeneration.value = task?.state.generation || null
  taskName.value = task?.name || ''; settings.value = copySettings(task?.state.settings || snapshot.value.defaults)
  filter.value = ''; editorError.value = ''
}
async function openEditor(task: Task | null, event: Event) {
  if (busy.value || !snapshot.value) return
  previousFocus = event.currentTarget as HTMLElement
  adopt(task); editorOpen.value = true
  await nextTick()
  dialog.value?.showModal()
  dialog.value?.querySelector<HTMLInputElement>('input[aria-label="任务名称"]')?.focus()
  void loadModels()
}
function restoreFocus() {
  if (!previousFocus || busy.value) return
  if (previousFocus.isConnected && !previousFocus.matches(':disabled')) previousFocus.focus()
  else refreshButton.value?.focus()
  previousFocus = null
}
async function editorClosed() {
  editorOpen.value = false
  await nextTick()
  restoreFocus()
}
watch(busy, async value => { if (!value && !editorOpen.value) { await nextTick(); restoreFocus() } })
async function reloadTask() {
  if (busy.value) return
  busy.value = true; editorError.value = ''
  try {
    snapshot.value = await request<Snapshot>('monitor')
    const task = snapshot.value.tasks.find(item => item.id === editingId.value)
    if (editingId.value !== null && !task) { editorError.value = '任务已不存在，请关闭后刷新列表'; return }
    adopt(task || null)
    await loadModels()
  } catch (e) { editorError.value = failure(e) } finally { busy.value = false }
}
async function save() {
  const value = settings.value
  if (busy.value || !value) return
  editorError.value = ''
  if (!taskName.value.trim()) { editorError.value = '请输入任务名称'; return }
  if (staleEditor.value) { editorError.value = '任务设置已变化，请重载任务后再保存'; return }
  if (!Number.isInteger(value.daily_max) || value.daily_max < 1 || value.daily_max > 1440) { editorError.value = '每日每账号上限须为1至1440的整数'; return }
  if (!Number.isInteger(value.interval_minutes) || value.interval_minutes < 5 || value.interval_minutes > 1440) { editorError.value = '测试间隔须为5至1440的整数分钟'; return }
  if (!Number.isInteger(value.confirmation_delay_seconds) || value.confirmation_delay_seconds < 1 || value.confirmation_delay_seconds > 86400) { editorError.value = '复测等待须为1至86400的整数秒'; return }
  if (value.account_ids.length > 50) { editorError.value = '每个任务最多选择50个账号'; return }
  if (modelsLoading.value) { editorError.value = '模型目录正在加载，请稍后保存'; return }
  busy.value = true; error.value = ''; notice.value = ''
  try {
    snapshot.value = await request<Snapshot>('monitor', { task_id: editingId.value, name: taskName.value.trim(), settings: copySettings(value), expected_generation: editGeneration.value })
    notice.value = '任务已保存，确认命中已清零，执行次数保留'
    dialog.value?.close()
  } catch (e) { editorError.value = failure(e) } finally { busy.value = false }
}
async function toggleScheduled(task: Task) {
  if (busy.value) return
  const value = copySettings(task.state.settings)
  value.scheduled = !value.scheduled
  if (!value.scheduled) value.auto_disable = false
  busy.value = true; error.value = ''; notice.value = ''
  try {
    snapshot.value = await request<Snapshot>('monitor', { task_id: task.id, name: task.name, settings: value, expected_generation: task.state.generation })
    notice.value = value.scheduled ? '任务已启用，暂停状态保留，首轮按所设间隔执行' : '任务已关闭，自动停用已关闭，已发出的操作不撤回'
  } catch (e) { error.value = failure(e) } finally { busy.value = false }
}
async function control(paused: boolean, task: Task | null = null, accountId: string | null = null) {
  if (busy.value || !snapshot.value) return
  busy.value = true; error.value = ''; notice.value = ''
  try {
    snapshot.value = await request<Snapshot>('monitor/control', { task_id: task?.id || null, account_id: accountId, paused, expected_generation: task?.state.generation || '', expected_control_revision: task?.state.control_revision ?? snapshot.value.control_revision })
    notice.value = paused ? '监控已暂停，已发出的操作不撤回，确认命中已清零' : accountId ? '账号已继续，仍受任务启用、暂停、时段及限额约束' : '监控已继续，未启用的任务不会执行，仍遵守时段及每日上限'
  } catch (e) { error.value = failure(e) } finally { busy.value = false }
}
onMounted(load)
</script>

<template>
  <section class="panel">
    <div class="section-head">
      <strong>定时探针与自动停用 <span v-if="snapshot" class="muted">{{ snapshot.tasks.length }} / 10 个任务</span></strong>
      <div class="inline">
        <button ref="refreshButton" :disabled="busy" @click="load">刷新状态</button>
        <button :disabled="busy || !snapshot?.tasks.length" @click="control(true)">暂停全部</button>
        <button :disabled="busy || !snapshot?.tasks.length" @click="control(false)">继续全部</button>
        <button class="primary" :disabled="busy || !snapshot || snapshot.tasks.length >= 10" @click="openEditor(null, $event)">创建任务</button>
      </div>
    </div>
    <div v-if="error" role="alert" class="alert error">{{ error }}</div>
    <div v-if="notice" role="status" class="alert success">{{ notice }}</div>
    <p v-if="snapshot" class="muted">累计执行 {{ snapshot.total_runs }} 次 · 今日执行 {{ snapshot.daily_runs }} 次 · 北京时间（含曾监控账号）</p>
    <p v-if="!snapshot" class="empty">{{ busy ? '正在加载任务' : '未加载监控状态，请刷新重试' }}</p>
    <p v-else-if="!snapshot.tasks.length" class="empty">暂无监控任务</p>
    <div v-else class="monitor-task-list">
      <article v-for="task in snapshot.tasks" :key="task.id" class="monitor-task">
        <div class="section-head">
          <div class="monitor-task-title"><strong>{{ task.name }}</strong> <span class="badge">{{ taskStatus(task) }}</span><small>{{ task.state.settings.model || '未选择模型' }} · 每 {{ task.state.settings.interval_minutes }} 分钟 · {{ task.state.settings.account_ids.length }} 个账号 · {{ task.state.settings.auto_disable ? '自动停用已开启' : '不自动停用' }}</small></div>
          <div class="inline">
            <button :disabled="busy" :aria-label="'编辑任务 ' + task.name" @click="openEditor(task, $event)">编辑</button>
            <button :disabled="busy" :aria-label="(task.state.settings.scheduled ? '关闭任务 ' : '启用任务 ') + task.name" @click="toggleScheduled(task)">{{ task.state.settings.scheduled ? '关闭任务' : '启用任务' }}</button>
            <button :disabled="busy || !task.state.settings.scheduled" :aria-label="(task.state.paused ? '继续任务 ' : '暂停任务 ') + task.name" @click="control(!task.state.paused, task)">{{ task.state.paused ? '继续' : '暂停' }}</button>
          </div>
        </div>
        <p class="muted">累计 {{ task.total_runs }} 次 · 今日 {{ task.daily_runs }} 次 · 最近后台检查 {{ time(task.state.last_tick_ms) }}</p>
        <div v-if="task.state.last_error" role="alert" class="alert warning">{{ task.state.last_error }}</div>
        <details>
          <summary>账号详情（{{ task.state.settings.account_ids.length }}）</summary>
          <div v-if="task.state.settings.account_ids.length" class="table-wrap monitor-accounts"><table><thead><tr><th>账号</th><th>监控状态</th><th>累计 / 今日次数</th><th>最近测试</th><th>结果</th><th>降级命中</th><th>下次可执行</th><th>账号处置</th><th>操作</th></tr></thead><tbody>
            <tr v-for="id in task.state.settings.account_ids" :key="id">
              <td>{{ name(id) }}</td><td>{{ task.availability[id]?.status || '尚未执行' }}</td>
              <td>{{ task.state.accounts[id]?.total_runs || 0 }} / {{ task.state.accounts[id]?.daily_runs || 0 }}<small>本任务今日上限 {{ task.state.settings.daily_max }}</small></td>
              <td>{{ time(task.state.accounts[id]?.last_checked_ms) }}</td><td>{{ statusLabel(task.state.accounts[id]?.last_status || '') }}</td>
              <td>{{ task.state.accounts[id]?.streak || 0 }} / {{ task.state.settings.degradation_policy === 'immediate' ? 1 : 2 }}</td>
              <td>{{ time(task.availability[id]?.next_allowed_ms) }}</td><td>{{ actionLabel(task.state.accounts[id]?.action || '') }}<small>{{ task.state.accounts[id]?.action_detail }}</small></td>
              <td><button :disabled="busy || !task.state.settings.scheduled" :aria-label="(task.state.accounts[id]?.paused ? '继续监控 ' : '暂停监控 ') + task.name + ' / ' + name(id)" @click="control(!task.state.accounts[id]?.paused, task, id)">{{ task.state.accounts[id]?.paused ? '继续' : '暂停' }}</button></td>
            </tr>
          </tbody></table></div>
          <p v-else class="empty">尚未配置监控账号</p>
        </details>
      </article>
    </div>
    <details class="question-preview"><summary>后台执行与安全边界</summary><div class="help">
      <p>最多10个任务，同一账号在多任务间串行执行，次数与每日限额按每任务每账号独立统计，旧计划升级后迁为暂停任务，请确认设置后继续</p>
      <p>关闭页面后，宿主维护任务仍继续执行，停用插件后停止，每次维护最多测试一个到期账号，繁忙时顺延</p>
      <p>预约确认由后续维护回调执行，等待不少于配置秒数，不保证精确到秒，确认等待期间不启动同账号的常规探针，确认到期后超过一个常规间隔或跨过不测试时段则作废，不以陈旧结果停用</p>
      <p>暂停或保存会清空确认链，不撤回已发出的请求，继续后按常规间隔执行，关闭任务同时关闭自动停用，重新启用后需编辑任务开启自动停用</p>
      <p>执行次数按后台准入次数统计，已准入但失败或取消仍计次，保存、暂停及进程重启不清空次数，每日上限包含确认复测，达到上限后次日恢复，不测试时段不补发堆积任务</p>
      <p>手动探针和题目测试不参与监控判定，超时、限流、认证失败或无法判断不触发自动停用，自动停用作用于宿主账号，也会影响其他任务</p>
      <p>定时探针最多等待18秒，手动探针最多90秒，停用动作未确认时不自动重发，需在账号管理核对，账号不会自动恢复</p>
      <p>探针按Key限定账号与模型范围，强制走账号代理，失败不直连，不计入Key账单，不自动刷新令牌，敏感导出仅限所选单账号和本次调用内存，敏感内容不返回页面、不写状态或日志</p>
    </div></details>
  </section>

  <dialog ref="dialog" class="dialog monitor-editor" aria-labelledby="monitor-editor-title" @close="editorClosed">
    <form v-if="editorOpen && settings" @submit.prevent="save">
      <div class="section-head"><strong id="monitor-editor-title">{{ editingId === null ? '创建监控任务' : '编辑监控任务' }}</strong><button type="button" aria-label="关闭任务编辑" @click="dialog?.close()">关闭</button></div>
      <div v-if="staleEditor" role="alert" class="alert warning">任务设置已变化，未保存内容已保留，请重载任务后再保存</div>
      <div v-if="editorError" role="alert" class="alert error">{{ editorError }}</div>
      <fieldset :disabled="busy">
        <label>任务名称<input v-model="taskName" required aria-label="任务名称"></label>
        <div class="inline monitor-toggles"><label class="choice"><input v-model="settings.scheduled" type="checkbox" @change="!settings.scheduled && (settings.auto_disable = false)">启用定时探针</label><label class="choice"><input v-model="settings.auto_disable" type="checkbox" :disabled="!settings.scheduled">启用自动停用账号</label></div>
        <div v-if="settings.auto_disable" class="alert warning">仅明确疑似降级时触发停用，{{ settings.degradation_policy === 'immediate' ? '立即停用不复测，误停风险较高' : '等待后复测仍疑似降级才停用' }}，账号不会自动恢复</div>
        <div class="form-grid">
          <label>监控客户端Key<select v-model="settings.client_key_id" aria-label="监控客户端Key" @change="loadModels"><option :value="null">请选择</option><option v-for="key in props.keys.filter(k => k.enabled || k.id === settings?.client_key_id)" :key="key.id" :value="key.id">{{ key.name || key.id }}{{ key.enabled ? '' : '（已停用）' }}</option></select></label>
          <label>测试间隔（分钟）<input v-model.number="settings.interval_minutes" type="number" min="5" max="1440" step="1"></label>
          <label>监控模型<select v-model="settings.model" aria-label="监控模型" :disabled="modelsLoading || !models.length"><option v-if="!models.length" :value="modelsError ? settings.model : ''">{{ modelsLoading ? '正在加载可见模型' : modelsError ? settings.model || '模型加载失败' : settings.client_key_id ? '暂无可见模型' : '请先选择Key' }}</option><option v-for="model in models" :key="model" :value="model">{{ model }}</option></select><small v-if="modelsError" role="alert">模型加载失败 {{ modelsError }} <button class="text-button" type="button" @click="loadModels">重试</button></small><small v-else-if="settings.client_key_id && !modelsLoading && !models.length">所选Key暂无可见模型，请在宿主检查模型范围</small></label>
        </div>
        <div class="form-grid"><label>降级处理策略<select v-model="settings.degradation_policy" aria-label="降级处理策略"><option value="confirm">延迟复测，仍疑似降级时停用</option><option value="immediate">首次疑似降级立即停用，不复测</option></select></label><label v-if="settings.degradation_policy === 'confirm'">复测等待（秒）<input v-model.number="settings.confirmation_delay_seconds" aria-label="复测等待（秒）" type="number" min="1" max="86400" step="1"><small>到期后由宿主维护回调执行，实际等待可能更长</small></label></div>
        <div class="form-grid"><label>每日每账号最大测试次数<input v-model.number="settings.daily_max" type="number" aria-label="每日每账号最大测试次数" min="1" max="1440" step="1"><small>一次探针任务计1次，确认复测另计1次</small></label><label class="choice"><input v-model="quietEnabled" type="checkbox">启用不测试时段</label><div v-if="settings.quiet_hours" class="inline"><label>不测试开始<input v-model="settings.quiet_hours.start" type="time" aria-label="不测试开始" required></label><label>不测试结束<input v-model="settings.quiet_hours.end" type="time" aria-label="不测试结束" required></label></div></div>
        <p class="muted">北京时间每日零点重置，时段可跨午夜，限制优先于复测，手动探针不计数、不触发自动停用</p>
        <div class="selection-header"><strong>监控账号 <span class="muted">已选 {{ settings.account_ids.length }} / 50</span></strong><div class="inline"><input v-model="filter" aria-label="筛选监控账号" placeholder="搜索名称、邮箱"><button type="button" @click="settings.account_ids = candidates.filter(a => a.enabled && a.proxy_configured).slice(0, 50).map(a => a.id)">选择筛选结果</button><button type="button" @click="settings.account_ids = []">取消选择</button></div></div>
        <div class="account-list"><label v-for="account in candidates" :key="account.id" class="choice"><input v-model="settings.account_ids" type="checkbox" :value="account.id" :disabled="!settings.account_ids.includes(account.id) && (!account.enabled || !account.proxy_configured || settings.account_ids.length >= 50)"><span>{{ account.name }}<small>{{ account.email && account.email !== account.name ? account.email + ' · ' : '' }}{{ account.enabled ? (account.proxy_configured ? 'OpenAI OAuth · 账号代理已配置' : '未配置账号代理') : '已停用，将跳过' }}</small></span></label><p v-if="!candidates.length" class="empty">暂无匹配的OpenAI OAuth账号</p></div>
      </fieldset>
      <div class="runbar"><button type="button" :disabled="busy" @click="reloadTask">{{ editingId === null ? '重载默认设置' : '重载任务' }}</button><div class="inline"><button type="button" @click="dialog?.close()">取消</button><button class="primary" type="submit" :disabled="busy || modelsLoading || staleEditor">{{ busy ? '处理中' : '保存任务' }}</button></div></div>
    </form>
  </dialog>
</template>

<style scoped>
.monitor-task { padding: 16px 0; border-top: 1px solid var(--border); }
.monitor-task .section-head { margin-bottom: 8px; }
.monitor-task-title { min-width: 0; overflow-wrap: anywhere; }
.monitor-task-title .badge { margin-left: 6px; }
.monitor-accounts { margin-top: 10px; }
.monitor-accounts table { min-width: 960px; }
.monitor-editor { color: var(--text); width: min(960px, calc(100% - 40px)); max-width: 960px; }
.monitor-editor::backdrop { background: #0006; }
.monitor-editor .form-grid { grid-template-columns: repeat(3, minmax(0, 1fr)); }
.help p { margin: 0 0 10px; }
.help p:last-child { margin-bottom: 0; }
@media (max-width: 900px) { .monitor-editor .form-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
@media (max-width: 560px) {
  .monitor-task .section-head { align-items: flex-start; }
  .monitor-editor { width: calc(100% - 20px); max-height: 90vh; padding: 12px; }
  .monitor-editor .form-grid { grid-template-columns: 1fr; }
  .monitor-editor .selection-header .inline { width: 100%; }
  .monitor-editor .selection-header input { width: 100%; }
}
</style>
