import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import vm from 'node:vm'

// 检查随包产物而非开发入口，开发服务器允许模块脚本，不代表宿主可加载。
test('管理页生产产物使用完整经典脚本及已声明资源', () => {
  const html = readFileSync(new URL('../../web/index.html', import.meta.url), 'utf8')
  const script = readFileSync(new URL('../../web/app.js', import.meta.url), 'utf8')
  const manifest = JSON.parse(readFileSync(new URL('../../plugin.json', import.meta.url), 'utf8'))
  assert.doesNotMatch(html, /type\s*=\s*["']module["']/i)
  assert.doesNotMatch(html, /modulepreload/i)
  assert.match(html, /<script\s+src="\.\/app\.js"><\/script>/)
  assert.match(html, /href="\.\/style\.css"/)
  assert.equal(manifest.resources['web/app.js'], 'text/javascript')
  assert.equal(manifest.resources['web/style.css'], 'text/css')
  // vm.Script 按经典脚本语法解析，顶层 import/export 将直接失败。
  assert.doesNotThrow(() => new vm.Script(script))
  assert.doesNotMatch(script, /\bimport\s*\(/)
  assert.doesNotMatch(script, /process\.env/)
})
