import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import ts from 'typescript'
const require = createRequire(import.meta.url)
let requestModels
const source = readFileSync(new URL('../src/models.ts', import.meta.url), 'utf8')
const code = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText
const exports = {}
new Function('require', 'exports', code)(name => name === './api' ? { request: (...args) => requestModels(...args) } : require(name), exports)
const { chooseModel, useModels } = exports
const { ref } = require('vue')
test('默认模型只从当前Key可见目录选择并保留有效选择', () => {
  assert.equal(chooseModel(['other', 'gpt-6-astra'], ''), 'gpt-6-astra')
  assert.equal(chooseModel(['other', 'gpt-6-astra'], 'other'), 'other')
  assert.equal(chooseModel(['only'], 'removed'), 'only')
  assert.equal(chooseModel([], 'removed'), '')
})
test('切换Key丢弃迟到响应，空Key清除模型和加载状态', async () => {
  const replies = new Map()
  requestModels = (path, body) => new Promise(resolve => replies.set(body.client_key_id, resolve))
  let key = 'first'; const model = ref(''), state = useModels(() => key, model)
  const first = state.loadModels(); key = 'second'; const second = state.loadModels()
  replies.get('second')({ models: ['second-model'] }); await second
  replies.get('first')({ models: ['first-model'] }); await first
  assert.deepEqual(state.models.value, ['second-model']); assert.equal(model.value, 'second-model')
  key = null; await state.loadModels()
  assert.equal(model.value, ''); assert.equal(state.modelsLoading.value, false)
})
test('模型加载失败可重试，空目录不伪造默认模型', async () => {
  requestModels = async () => { throw new Error('fixture-error') }
  const model = ref('old'), state = useModels(() => 'key', model)
  await state.loadModels(); assert.equal(state.modelsError.value, 'fixture-error'); assert.equal(model.value, 'old')
  requestModels = async () => ({ models: [] })
  await state.loadModels(); assert.equal(state.modelsError.value, ''); assert.equal(model.value, '')
  requestModels = async () => ({ models: ['available'] })
  await state.loadModels(); assert.equal(model.value, 'available')
})

test('过期失败与finally不会清除新Key的加载状态', async () => {
  const replies = new Map()
  requestModels = (path, body) => new Promise((resolve, reject) => replies.set(body.client_key_id, { resolve, reject }))
  let key = 'first'; const model = ref('old'), state = useModels(() => key, model)
  const first = state.loadModels(); key = 'second'; const second = state.loadModels()
  replies.get('first').reject(new Error('stale-error')); await first
  assert.equal(state.modelsError.value, ''); assert.equal(state.modelsLoading.value, true)
  assert.equal(model.value, 'old')
  replies.get('second').resolve({ models: ['second-model'] }); await second
  assert.equal(state.modelsLoading.value, false); assert.equal(model.value, 'second-model')
})
