import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

export default defineConfig({
  plugins: [vue()],
  base: './',
  define: { 'process.env.NODE_ENV': JSON.stringify('production') },
  build: {
    outDir: '../web',
    emptyOutDir: true,
    // 宿主页组装器将资源转换为 data URL，只接受不依赖模块加载的经典脚本。
    lib: {
      entry: 'src/main.ts',
      name: 'ModelQualityTest',
      formats: ['iife'],
      fileName: () => 'app.js',
      cssFileName: 'style',
    },
  },
})
