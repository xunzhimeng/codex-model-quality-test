import { ref, type Ref } from 'vue'
import { request } from './api'

export function chooseModel(models: string[], current: string) {
  if (models.includes(current)) return current
  return models.includes('gpt-6-astra') ? 'gpt-6-astra' : models[0] || ''
}

export function useModels(key: () => string | null, model: Ref<string>) {
  const models = ref<string[]>([]), modelsLoading = ref(false), modelsError = ref('')
  let version = 0
  async function loadModels() {
    const current = ++version, keyId = key()
    models.value = []; modelsError.value = ''; modelsLoading.value = false
    if (!keyId) { model.value = ''; return }
    modelsLoading.value = true
    try {
      const result = await request<{ models: string[] }>('models', { client_key_id: keyId })
      // Key切换后迟到的响应不能覆盖当前目录和默认值
      if (current !== version) return
      models.value = result.models
      model.value = chooseModel(result.models, model.value)
    } catch (e) {
      if (current === version) modelsError.value = (e as Error).message
    } finally { if (current === version) modelsLoading.value = false }
  }
  return { models, modelsLoading, modelsError, loadModels }
}
