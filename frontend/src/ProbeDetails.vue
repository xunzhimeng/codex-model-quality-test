<script setup lang="ts">
import { computed } from 'vue'
import { attemptGroups, attemptSummary, failureLabel, failureHints, resultView, type Attempt, type ProbeMetrics } from './result'
const props = defineProps<{ metrics: Record<string, unknown> }>()
const metrics = computed(() => props.metrics as ProbeMetrics)
const groups = computed(() => attemptGroups(metrics.value))
const summary = computed(() => attemptSummary(metrics.value))
const unlistedFailure = computed(() => groups.value.some(group => group.attempt?.failure) ? null : metrics.value.failure)
function attemptResult(attempt: Attempt) { return resultView({ status: attempt.status, metrics: { failure: attempt.failure } }) }
function action(value?: string) { return ({ disabled: '账号已自动停用', pending: '停用结果待确认', unconfirmed: '停用未确认，请检查账号状态' } as Record<string, string>)[value || ''] || '未触发停用' }
</script>
<template>
  <div class="probe-details">
    <p class="muted">来源：{{ metrics.source === 'scheduled' ? '后台定时探针' : '手动探针' }}{{ metrics.monitor?.confirmation ? '（确认复测）' : '' }} · 门票与Cookie只展示存在性和长度，不保存原文</p>
    <p v-if="metrics.monitor?.task_name" class="muted">监控任务：{{ metrics.monitor.task_name }}</p>
    <p v-if="metrics.transport === 'direct'" class="muted">网络路径：服务器直连 · Key仅限定范围，不计入账单</p>
    <p v-else-if="metrics.transport === 'account_proxy'" class="muted">网络路径：账号代理 · Key仅限定范围，不计入账单</p>
    <p v-if="summary">{{ summary }}</p>
    <p v-if="unlistedFailure" class="alert error">{{ failureLabel(unlistedFailure) }}<small v-if="unlistedFailure.message !== failureLabel(unlistedFailure)">{{ unlistedFailure.message }}</small><small v-if="failureHints(unlistedFailure)">{{ failureHints(unlistedFailure) }}</small></p>
    <section v-for="(group, groupIndex) in groups" :key="group.number ?? `legacy-${groupIndex}`">
      <h4 v-if="group.number !== null">第 {{ group.number }} 次尝试 <template v-if="group.attempt"><span class="badge" :class="attemptResult(group.attempt).className">{{ attemptResult(group.attempt).label }}</span> <span class="muted">{{ (group.attempt.elapsed_ms / 1000).toFixed(2) }} s</span></template></h4>
      <h4 v-else-if="groups.length > 1">未标注尝试的轮次</h4>
      <p v-if="group.attempt?.failure" role="note" class="alert error">{{ group.attempt.failure.message || failureLabel(group.attempt.failure) }}<small v-if="failureHints(group.attempt.failure)">{{ failureHints(group.attempt.failure) }}</small></p>
      <div v-if="group.rounds.length" class="round-grid">
        <section v-for="(round, roundIndex) in group.rounds" :key="`${round.phase}-${roundIndex}`" class="round-card">
          <strong>{{ round.phase }}</strong>
          <dl><dt>HTTP状态</dt><dd>{{ round.http_status ?? '未收到响应' }}</dd><dt>耗时</dt><dd>{{ (round.elapsed_ms / 1000).toFixed(2) }} s</dd><dt>响应流</dt><dd>{{ round.completed ? '完整成功' : '未完整成功' }} · {{ round.response_bytes }} 字节</dd><dt>门票</dt><dd>{{ round.ticket_present ? `已返回 · ${round.ticket_length} 字节` : '未返回' }}</dd><dt>路由Cookie</dt><dd>{{ round.routing_cookie_count }} 项</dd><dt>回报模型</dt><dd>{{ round.reported_model || '未提供' }}</dd></dl>
          <p v-if="round.error" role="note" class="alert error">{{ round.error }}</p>
        </section>
      </div>
      <p v-else class="muted">本次未取得分轮详情</p>
    </section>
    <p v-if="!groups.length" class="muted">本次未取得分轮详情，旧版记录不补充推测信息</p>
    <p v-if="typeof metrics.new_ticket === 'boolean'">续接门票：{{ metrics.new_ticket ? '与首轮不同' : '与首轮相同或未返回新门票' }}</p>
    <p v-if="metrics.criterion" class="muted">{{ metrics.criterion }}</p>
    <div v-if="metrics.monitor" class="alert" :class="metrics.monitor.action ? 'warning' : 'success'">{{ action(metrics.monitor.action) }}<span v-if="metrics.monitor.streak !== undefined"> · 降级命中 {{ metrics.monitor.streak }} / {{ metrics.monitor.threshold ?? 2 }}</span><small v-if="metrics.monitor.detail">{{ metrics.monitor.detail }}</small></div>
  </div>
</template>
