<script setup lang="ts">
interface Round { phase: string; http_status: number | null; elapsed_ms: number; completed: boolean; response_bytes: number; ticket_present: boolean; ticket_length: number; routing_cookie_count: number; reported_model: string | null; error: string | null }
interface Metrics { rounds?: Round[]; source?: string; transport?: string; client_key_id?: string; key_billed?: boolean; criterion?: string; new_ticket?: boolean; monitor?: { streak?: number; threshold?: number; action?: string; detail?: string } }
defineProps<{ metrics: Metrics }>()
function action(value?: string) { return ({ disabled: '账号已自动停用', pending: '停用结果待确认', unconfirmed: '停用未确认，请检查账号状态' } as Record<string, string>)[value || ''] || '未触发停用' }
</script>
<template>
  <div class="probe-details">
    <p class="muted">来源：{{ metrics.source === 'scheduled' ? '后台定时探针' : '手动探针' }} · 门票与Cookie只展示存在性和长度，不保存原文</p>
    <p v-if="metrics.transport === 'account_proxy'" class="muted">网络路径：账号代理 · Key仅限定范围，不计入账单</p>
    <div v-if="metrics.rounds?.length" class="round-grid">
      <section v-for="round in metrics.rounds" :key="round.phase" class="round-card">
        <strong>{{ round.phase }}</strong>
        <dl><dt>HTTP状态</dt><dd>{{ round.http_status ?? '未收到响应' }}</dd><dt>耗时</dt><dd>{{ (round.elapsed_ms / 1000).toFixed(2) }} s</dd><dt>响应流</dt><dd>{{ round.completed ? '完整成功' : '未完整成功' }} · {{ round.response_bytes }} 字节</dd><dt>门票</dt><dd>{{ round.ticket_present ? `已返回 · ${round.ticket_length} 字节` : '未返回' }}</dd><dt>路由Cookie</dt><dd>{{ round.routing_cookie_count }} 项</dd><dt>回报模型</dt><dd>{{ round.reported_model || '未提供' }}</dd></dl>
        <p v-if="round.error" role="note" class="alert error">{{ round.error }}</p>
      </section>
    </div>
    <p v-else class="muted">本次未取得分轮详情；旧版记录不补充推测信息</p>
    <p v-if="typeof metrics.new_ticket === 'boolean'">续接门票：{{ metrics.new_ticket ? '与首轮不同' : '与首轮相同或未返回新门票' }}</p>
    <p v-if="metrics.criterion" class="muted">{{ metrics.criterion }}</p>
    <div v-if="metrics.monitor" class="alert" :class="metrics.monitor.action ? 'warning' : 'success'">{{ action(metrics.monitor.action) }}<span v-if="metrics.monitor.streak !== undefined"> · 连续命中 {{ metrics.monitor.streak }} / 2</span><small v-if="metrics.monitor.detail">{{ metrics.monitor.detail }}</small></div>
  </div>
</template>
