import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
export default defineConfig({ plugins: [vue()], base: './', build: { outDir: '../web', emptyOutDir: true, rollupOptions: { output: { entryFileNames: 'app.js', chunkFileNames: 'chunk-[name].js', assetFileNames: asset => asset.names.some(name => name.endsWith('.css')) ? 'style.css' : '[name][extname]' } } } })
