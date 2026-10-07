import test from 'node:test'
import assert from 'node:assert/strict'
import { runAccountQueue, csvCell } from '../src/queue.ts'
test('账号串行、跨账号有界并发', async () => {
  const active = new Set(), order = new Map(); let maximum = 0
  await runAccountQueue(['a','b','c'], [1,2,3], 2, async (account, task) => {
    assert.ok(!active.has(account)); active.add(account); maximum = Math.max(maximum, active.size)
    await new Promise(resolve => setImmediate(resolve))
    order.set(account, [...(order.get(account) || []), task]); active.delete(account)
  }, () => false)
  assert.equal(maximum, 2)
  for (const list of order.values()) assert.deepEqual(list, [1,2,3])
})
test('停止不启动后续任务', async () => {
  let stop = false; const sent = []
  await runAccountQueue(['a','b'], [1,2,3], 1, async (account, task) => { sent.push([account,task]); stop = true }, () => stop)
  assert.deepEqual(sent, [['a',1]])
})
test('CSV防公式注入与正确转义', () => {
  assert.equal(csvCell('=1+2'), '"\'=1+2"')
  assert.equal(csvCell('  @SUM(1)'), '"\'  @SUM(1)"')
  assert.equal(csvCell('a"b'), '"a""b"')
  assert.equal(csvCell(null), '""')
})
