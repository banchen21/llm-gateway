<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { Refresh } from '@element-plus/icons-vue'
import { fetchLogs, type LogEntry } from '../api/client'

const entries = ref<LogEntry[]>([])
const loading = ref(false)
const autoRefresh = ref(false)
const error = ref('')

let timer: number | undefined

const totals = computed(() => {
  const usage = entries.value.reduce(
    (sum, entry) => ({
      input: sum.input + (entry.usage?.input_tokens ?? 0),
      output: sum.output + (entry.usage?.output_tokens ?? 0),
      cached: sum.cached + (entry.usage?.cache_read_tokens ?? 0),
    }),
    { input: 0, output: 0, cached: 0 },
  )
  const failures = entries.value.filter((entry) => entry.status >= 400).length
  return { ...usage, failures }
})

onMounted(refresh)
onUnmounted(stopAutoRefresh)

async function refresh() {
  loading.value = true
  try {
    entries.value = (await fetchLogs(300)).entries
    error.value = ''
  } catch (problem) {
    error.value = problem instanceof Error ? problem.message : String(problem)
  } finally {
    loading.value = false
  }
}

function toggleAutoRefresh(enabled: boolean) {
  autoRefresh.value = enabled
  if (enabled) {
    timer = window.setInterval(refresh, 3000)
  } else {
    stopAutoRefresh()
  }
}

function stopAutoRefresh() {
  if (timer !== undefined) {
    window.clearInterval(timer)
    timer = undefined
  }
}

function timestamp(at: number): string {
  return new Date(at).toLocaleString('zh-CN', { hour12: false })
}

function formatTokens(count: number): string {
  return count ? count.toLocaleString('en-US') : '—'
}
</script>

<template>
  <div>
    <div class="gw-section">
      <span class="gw-section-title">请求记录</span>
      <span class="gw-section-note">
        只记录用量与请求形状，不记录消息内容。流式请求会在响应结束后才落盘。
      </span>
      <div class="grow" />
      <el-switch
        :model-value="autoRefresh"
        size="small"
        @update:model-value="toggleAutoRefresh"
      />
      <span class="gw-hint">自动刷新</span>
      <el-button :icon="Refresh" size="small" :loading="loading" @click="refresh">刷新</el-button>
    </div>

    <el-alert
      v-if="error"
      type="error"
      show-icon
      :closable="false"
      title="读取日志失败"
      :description="error"
      class="mb"
    />

    <div class="summary">
      <div class="stat">
        <span class="stat-label">记录数</span>
        <span class="stat-value mono">{{ entries.length }}</span>
      </div>
      <div class="stat">
        <span class="stat-label">失败</span>
        <span class="stat-value mono" :class="{ bad: totals.failures > 0 }">
          {{ totals.failures }}
        </span>
      </div>
      <div class="stat">
        <span class="stat-label">输入 tokens</span>
        <span class="stat-value mono">{{ formatTokens(totals.input) }}</span>
      </div>
      <div class="stat">
        <span class="stat-label">输出 tokens</span>
        <span class="stat-value mono">{{ formatTokens(totals.output) }}</span>
      </div>
      <div class="stat">
        <span class="stat-label">命中缓存</span>
        <span class="stat-value mono">{{ formatTokens(totals.cached) }}</span>
      </div>
    </div>

    <div v-if="!entries.length && !loading" class="gw-empty">
      还没有请求记录。配置好供应商后，用 Anthropic 或 OpenAI 客户端向网关发一次请求就会出现。
    </div>

    <el-table v-else :data="entries" size="small" border max-height="620">
      <el-table-column label="时间" width="164">
        <template #default="{ row }">
          <span class="mono">{{ timestamp(row.at) }}</span>
        </template>
      </el-table-column>

      <el-table-column label="状态" width="76" align="center">
        <template #default="{ row }">
          <el-tag :type="row.status < 400 ? 'success' : 'danger'" size="small" effect="light">
            {{ row.status }}
          </el-tag>
        </template>
      </el-table-column>

      <el-table-column label="模型别名" min-width="150">
        <template #default="{ row }">
          <span class="mono">{{ row.model }}</span>
        </template>
      </el-table-column>

      <el-table-column label="供应商" min-width="120" prop="provider" />

      <el-table-column label="上游模型" min-width="150">
        <template #default="{ row }">
          <span class="mono">{{ row.upstream_model }}</span>
        </template>
      </el-table-column>

      <el-table-column label="方式" width="72" align="center">
        <template #default="{ row }">
          <el-tag :type="row.stream ? 'primary' : 'info'" size="small" effect="plain">
            {{ row.stream ? '流式' : '整包' }}
          </el-tag>
        </template>
      </el-table-column>

      <el-table-column label="输入" width="88" align="right">
        <template #default="{ row }">
          <span class="mono">{{ formatTokens(row.usage?.input_tokens ?? 0) }}</span>
        </template>
      </el-table-column>

      <el-table-column label="输出" width="88" align="right">
        <template #default="{ row }">
          <span class="mono">{{ formatTokens(row.usage?.output_tokens ?? 0) }}</span>
        </template>
      </el-table-column>

      <el-table-column label="缓存读" width="88" align="right">
        <template #default="{ row }">
          <span class="mono">{{ formatTokens(row.usage?.cache_read_tokens ?? 0) }}</span>
        </template>
      </el-table-column>

      <el-table-column type="expand">
        <template #default="{ row }">
          <div class="detail">
            <div class="detail-row">
              <span class="gw-hint">提示词形状</span>
              <span class="mono">
                系统 {{ row.prompt.system_chars }} 字符 · {{ row.prompt.messages }} 条消息 ·
                {{ row.prompt.tool_results }} 个工具结果 · {{ row.prompt.tools }} 个工具定义
              </span>
            </div>
            <div v-if="row.usage" class="detail-row">
              <span class="gw-hint">用量明细</span>
              <span class="mono">
                输入 {{ row.usage.input_tokens }} · 输出 {{ row.usage.output_tokens }} ·
                缓存读 {{ row.usage.cache_read_tokens }} · 缓存写 {{ row.usage.cache_write_tokens }}
              </span>
            </div>
            <div v-if="row.error" class="detail-row">
              <span class="gw-hint">错误</span>
              <span class="mono error-text">{{ row.error }}</span>
            </div>
            <div v-if="row.status >= 400 && !row.usage" class="gw-hint">
              这次请求没有成功送达上游，因此没有用量数据。
            </div>
          </div>
        </template>
      </el-table-column>
    </el-table>
  </div>
</template>

<style scoped>
.grow {
  flex: 1;
}

.mb {
  margin-bottom: 16px;
}

.summary {
  display: flex;
  flex-wrap: wrap;
  gap: 28px;
  padding: 14px 18px;
  margin-bottom: 16px;
  background: var(--gw-surface);
  border: 1px solid var(--gw-line);
  border-radius: 6px;
}

.stat {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.stat-label {
  color: var(--gw-muted);
  font-size: 11px;
  letter-spacing: 0.03em;
}

.stat-value {
  font-size: 16px;
  font-weight: 600;
}

.stat-value.bad {
  color: var(--el-color-danger);
}

.detail {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 12px 16px;
  background: var(--gw-canvas);
  border-radius: 6px;
}

.detail-row {
  display: flex;
  align-items: baseline;
  gap: 16px;
}

.detail-row .gw-hint {
  flex: 0 0 72px;
}

.error-text {
  color: var(--el-color-danger);
  word-break: break-all;
}
</style>
