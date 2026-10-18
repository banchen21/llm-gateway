<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Document, Grid, Reading, Refresh, Setting } from '@element-plus/icons-vue'
import { fetchHealth, getToken, setToken, type Health } from './api/client'
import { useGateway } from './store'
import { PROTOCOL_MODE_LABELS } from './api/types'

const gateway = useGateway()
const route = useRoute()
const health = ref<Health | null>(null)
const tokenDraft = ref(getToken())
const tokenOpen = ref(false)

const title = computed(() => (route.meta.title as string | undefined) ?? 'LLM Gateway')

const statusTag = computed(() => {
  if (gateway.loadError.value) return { type: 'danger' as const, text: '未连接' }
  if (!health.value) return { type: 'info' as const, text: '检查中' }
  const { providers, aliases } = health.value
  if (providers === 0) return { type: 'warning' as const, text: '尚未配置供应商' }
  return { type: 'success' as const, text: `${providers} 个供应商 · ${aliases} 个模型别名` }
})

onMounted(async () => {
  await refresh()
  await gateway.load()
})

async function refresh() {
  try {
    health.value = await fetchHealth()
  } catch {
    health.value = null
  }
}

async function save() {
  const problem = await gateway.save()
  if (problem) {
    ElMessage.error(problem)
  } else {
    ElMessage.success('已保存并生效')
  }
  await refresh()
}

async function revert() {
  await ElMessageBox.confirm('放弃当前未保存的修改？', '确认', {
    confirmButtonText: '放弃修改',
    cancelButtonText: '继续编辑',
    type: 'warning',
  })
  await gateway.revert()
  await refresh()
}

function applyToken() {
  setToken(tokenDraft.value.trim())
  tokenOpen.value = false
  void gateway.load().then(refresh)
}
</script>

<template>
  <el-container class="shell">
    <el-aside width="184px" class="nav">
      <div class="brand">
        <span class="brand-mark">LG</span>
        <div class="brand-text">
          <strong>LLM Gateway</strong>
          <span class="mono">{{ health?.protocol_mode ?? '—' }}</span>
        </div>
      </div>
      <el-menu :default-active="route.path" router>
        <el-menu-item index="/providers">
          <el-icon><Grid /></el-icon>
          <span>供应商</span>
        </el-menu-item>
        <el-menu-item index="/mode">
          <el-icon><Setting /></el-icon>
          <span>模式与端口</span>
        </el-menu-item>
        <el-menu-item index="/logs">
          <el-icon><Document /></el-icon>
          <span>请求记录</span>
        </el-menu-item>
        <el-menu-item index="/guide">
          <el-icon><Reading /></el-icon>
          <span>使用说明</span>
        </el-menu-item>
      </el-menu>
      <div class="nav-foot">
        <el-popover :visible="tokenOpen" placement="right" :width="280" trigger="click">
          <template #reference>
            <el-button size="small" text @click="tokenOpen = !tokenOpen">
              管理令牌
              <el-tag v-if="getToken()" size="small" type="success" class="token-flag">已设</el-tag>
            </el-button>
          </template>
          <p class="gw-hint">
            仅当「模式与端口」里开启了 Web 界面并配置了令牌时才需要填写，用于访问管理接口。
          </p>
          <el-input
            v-model="tokenDraft"
            size="small"
            placeholder="粘贴网关令牌"
            show-password
            @keyup.enter="applyToken"
          />
          <div class="token-actions">
            <el-button size="small" type="primary" @click="applyToken">应用</el-button>
          </div>
        </el-popover>
      </div>
    </el-aside>

    <el-container>
      <el-header height="56px" class="topbar">
        <h1 class="page-title">{{ title }}</h1>
        <el-tag :type="statusTag.type" size="small" effect="light">{{ statusTag.text }}</el-tag>
        <el-tag v-if="health" size="small" effect="plain">
          {{ PROTOCOL_MODE_LABELS[health.protocol_mode] }}
        </el-tag>
        <div class="topbar-space" />
        <el-button :icon="Refresh" size="small" text @click="refresh">刷新</el-button>
        <el-button v-if="gateway.dirty.value" size="small" @click="revert">放弃</el-button>
        <el-button
          type="primary"
          size="small"
          :loading="gateway.saving.value"
          :disabled="!gateway.dirty.value"
          @click="save"
        >
          保存并生效
        </el-button>
      </el-header>

      <el-main class="content">
        <el-alert
          v-if="gateway.loadError.value"
          type="error"
          show-icon
          :closable="false"
          title="无法读取配置"
          :description="gateway.loadError.value"
          class="mb"
        />
        <el-skeleton v-if="gateway.loading.value" :rows="6" animated />
        <router-view v-else />
      </el-main>
    </el-container>
  </el-container>
</template>

<style scoped>
.shell {
  height: 100%;
}

.nav {
  display: flex;
  flex-direction: column;
  background: var(--gw-surface);
  border-right: 1px solid var(--gw-line);
}

.brand {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 14px 16px;
  border-bottom: 1px solid var(--gw-line);
}

.brand-mark {
  display: grid;
  place-items: center;
  width: 28px;
  height: 28px;
  border-radius: 6px;
  background: var(--gw-accent);
  color: #fff;
  font-size: 12px;
  font-weight: 700;
  letter-spacing: 0.02em;
}

.brand-text {
  display: flex;
  flex-direction: column;
  line-height: 1.25;
}

.brand-text strong {
  font-size: 13px;
}

.brand-text .mono {
  color: var(--gw-muted);
  font-size: 11px;
}

.nav :deep(.el-menu) {
  border-right: none;
  flex: 1;
}

.nav-foot {
  padding: 8px 12px;
  border-top: 1px solid var(--gw-line);
}

.token-flag {
  margin-left: 6px;
}

.token-actions {
  margin-top: 8px;
  text-align: right;
}

.topbar {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 0 20px;
  background: var(--gw-surface);
  border-bottom: 1px solid var(--gw-line);
}

.page-title {
  margin: 0;
  font-size: 15px;
  font-weight: 600;
}

.topbar-space {
  flex: 1;
}

.content {
  padding: 20px;
  overflow: auto;
}

.mb {
  margin-bottom: 16px;
}
</style>
