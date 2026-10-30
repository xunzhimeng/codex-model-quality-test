import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const monitor = readFileSync(new URL('../src/MonitorPanel.vue', import.meta.url), 'utf8')
const app = readFileSync(new URL('../src/App.vue', import.meta.url), 'utf8')

test('监控详情默认展开，任务元数据同行且保留真实错误', () => {
  assert.match(monitor, /<details open>\s*<summary>账号详情/)
  assert.match(monitor, /class="monitor-task-title"/)
  assert.match(monitor, /class="monitor-result"/)
  assert.match(monitor, /display: inline-flex; align-items: center; gap: 8px/)
  assert.ok(monitor.includes('task.state.last_error'))
  assert.ok(monitor.includes('accountResult(task.state.accounts[id]).attempts'))
  assert.doesNotMatch(monitor, /本任务今日上限|最近后台检查|无停用动作|自动停用已开启/)
})

test('监控成功提示使用紧凑状态行，不显示旧手动批次横幅', () => {
  assert.match(app, /v-if="notice && tab !== 'monitor'"/)
  assert.match(monitor, /class="monitor-summary muted"/)
  assert.doesNotMatch(monitor, /class="alert success"/)
  assert.ok(monitor.includes('已保存，复测命中清零'))
  assert.ok(monitor.includes('清空确认链，不撤回已发请求'))
})
