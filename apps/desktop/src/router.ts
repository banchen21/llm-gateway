import { createRouter, createWebHashHistory } from 'vue-router'

/**
 * Hash history, because one build is served from three places — the gateway's own port,
 * Vite's dev server, and the desktop shell's asset protocol — and only the first can be
 * configured to fall back to the index for deep links.
 */
export const router = createRouter({
  history: createWebHashHistory(),
  routes: [
    { path: '/', redirect: '/providers' },
    {
      path: '/providers',
      component: () => import('./views/ProvidersView.vue'),
      meta: { title: '供应商' },
    },
    {
      path: '/mode',
      component: () => import('./views/ModeView.vue'),
      meta: { title: '模式与端口' },
    },
    {
      path: '/logs',
      component: () => import('./views/LogsView.vue'),
      meta: { title: '请求记录' },
    },
    {
      path: '/benchmark',
      component: () => import('./views/BenchmarkView.vue'),
      meta: { title: '测试速率' },
    },
    {
      path: '/guide',
      component: () => import('./views/GuideView.vue'),
      meta: { title: '使用说明' },
    },
  ],
})
