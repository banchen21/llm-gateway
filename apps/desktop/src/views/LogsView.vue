<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { ElMessage, ElMessageBox, type TableInstance } from 'element-plus'
import { Delete, Refresh } from '@element-plus/icons-vue'
import { clearLogs, fetchLogs, type LogEntry } from '../api/client'

const entries = ref<LogEntry[]>([])
const table = ref<TableInstance>()
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

/** Share of the input tokens that came from the upstream's prompt cache. */
const cacheRate = computed(() => {
  if (!totals.value.input) return null
  return (totals.value.cached / totals.value.input) * 100
})

function formatRate(rate: number | null): string {
  return rate === null ? '—' : `${rate.toFixed(1)}%`
}

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

/** Failed calls carry a coloured rail on the left edge, so they scan without reading. */
function rowClass({ row }: { row: LogEntry }): string {
  return row.status >= 400 ? 'failed' : ''
}

/**
 * The whole row toggles its detail, not just the arrow.
 *
 * A click on the arrow is ignored here because the table already handles it; without the
 * guard the two would cancel each other out.
 */
function toggleRow(row: LogEntry, _column: unknown, event: Event) {
  const target = event.target as HTMLElement | null
  if (target?.closest('.el-table__expand-icon')) return
  table.value?.toggleRowExpansion(row)
}

async function clearRecords() {
  try {
    await ElMessageBox.confirm(
      '清除后无法恢复，只影响请求记录，不影响配置。',
      '清除请求记录',
      { type: 'warning', confirmButtonText: '清除', cancelButtonText: '取消' },
    )
  } catch {
    return
  }
  try {
    const { cleared } = await clearLogs()
    ElMessage.success(`已清除 ${cleared} 条记录`)
    await refresh()
  } catch (problem) {
    ElMessage.error(problem instanceof Error ? problem.message : String(problem))
  }
}
</script>

<template>
  <div>
    <div class="gw-section">
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
      <el-button :icon="Delete" size="small" text @click="clearRecords">清除记录</el-button>
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

    <div class="readout">
      <div class="metric">
        <span class="gw-micro">记录数</span>
        <span class="value mono">{{ entries.length }}</span>
      </div>
      <div class="metric">
        <span class="gw-micro">失败</span>
        <span class="value mono" :class="{ bad: totals.failures > 0 }">{{ totals.failures }}</span>
      </div>
      <div class="metric">
        <span class="gw-micro">输入 TOKENS</span>
        <span class="value mono">{{ formatTokens(totals.input) }}</span>
      </div>
      <div class="metric">
        <span class="gw-micro">输出 TOKENS</span>
        <span class="value mono">{{ formatTokens(totals.output) }}</span>
      </div>
      <div class="metric">
        <span class="gw-micro">命中缓存</span>
        <span class="value mono">{{ formatTokens(totals.cached) }}</span>
        <span class="sub">命中率 {{ formatRate(cacheRate) }}</span>
      </div>
    </div>

    <div v-if="!entries.length && !loading" class="gw-empty">
      还没有请求记录。配置好供应商后，用 Anthropic 或 OpenAI 客户端向网关发一次请求就会出现。
    </div>

    <el-table
      v-else
      ref="table"
      :data="entries"
      size="small"
      max-height="620"
      :row-class-name="rowClass"
      @row-click="toggleRow"
    >
      <el-table-column label="时间" width="158">
        <template #default="{ row }">
          <span class="mono time">{{ timestamp(row.at) }}</span>
        </template>
      </el-table-column>

      <el-table-column label="状态" width="88">
        <template #default="{ row }">
          <span class="status" :class="row.status < 400 ? 'ok' : 'bad'">
            <i class="dot" />{{ row.status }}
          </span>
        </template>
      </el-table-column>

      <el-table-column label="模型别名" min-width="150">
        <template #default="{ row }">
          <span class="mono">{{ row.model }}</span>
        </template>
      </el-table-column>

      <el-table-column label="供应商" min-width="186" show-overflow-tooltip>
        <template #default="{ row }">
          <span class="provider">{{ row.provider }}</span>
        </template>
      </el-table-column>

      <el-table-column label="上游模型" min-width="215" show-overflow-tooltip>
        <template #default="{ row }">
          <span class="mono upstream">{{ row.upstream_model }}</span>
        </template>
      </el-table-column>

      <el-table-column label="方式" width="76">
        <template #default="{ row }">
          <span class="chip" :class="{ stream: row.stream }">{{ row.stream ? '流式' : '整包' }}</span>
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

/* One bar of readouts divided by hairlines, the way an instrument panel stacks gauges. */
.readout {
  display: flex;
  margin-bottom: 16px;
  background: var(--gw-surface);
  border: 1px solid var(--gw-line);
  border-radius: 10px;
  overflow: hidden;
}

.metric {
  display: flex;
  flex: 1;
  flex-direction: column;
  gap: 3px;
  padding: 11px 18px;
  min-width: 0;
}

.metric + .metric {
  border-left: 1px solid var(--gw-line);
}

.value {
  font-size: 19px;
  font-weight: 500;
  line-height: 1.15;
  color: var(--gw-ink);
}

.value.bad {
  color: var(--gw-danger);
}

.sub {
  font-size: 11px;
  color: var(--gw-muted);
}

.time {
  color: var(--gw-slate);
  white-space: nowrap;
}

.status {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-family: var(--gw-mono);
  font-size: 12px;
}

.status .dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: currentColor;
}

.status.ok {
  color: var(--gw-accent);
}

.status.bad {
  color: var(--gw-danger);
}

.provider {
  color: var(--gw-slate);
  white-space: nowrap;
}

.upstream {
  color: var(--gw-slate);
  white-space: nowrap;
}

.chip {
  display: inline-block;
  padding: 0 6px;
  border: 1px solid var(--gw-line);
  border-radius: 4px;
  font-size: 11px;
  line-height: 18px;
  color: var(--gw-muted);
}

.chip.stream {
  color: var(--gw-accent);
  background: var(--gw-accent-tint);
  border-color: #cfe0da;
}

/* The failure rail: a 3px mark on the row's leading edge. */
:deep(.el-table__row) {
  cursor: pointer;
}

:deep(.el-table__row.failed td:first-child) {
  box-shadow: inset 3px 0 0 var(--gw-danger);
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
