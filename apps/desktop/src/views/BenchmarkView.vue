<script setup lang="ts">
import { computed, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { VideoPlay } from '@element-plus/icons-vue'
import { gatewayBase } from '../api/client'
import { useGateway } from '../store'

const gateway = useGateway()
const { aliases } = gateway

interface Run {
  index: number
  ok: boolean
  status: number
  ttfb: number
  total: number
  outputTokens: number | null
  error?: string
}

const model = ref('')
const runs = ref(5)
const stream = ref(true)
const apiKey = ref('')
const running = ref(false)
const results = ref<Run[]>([])

/** Median is the honest summary here: one stalled call would drag an average away. */
function median(values: number[]): number | null {
  if (!values.length) return null
  const sorted = [...values].sort((a, b) => a - b)
  const middle = Math.floor(sorted.length / 2)
  return sorted.length % 2
    ? sorted[middle]!
    : (sorted[middle - 1]! + sorted[middle]!) / 2
}

const successes = computed(() => results.value.filter((run) => run.ok))
const medianTotal = computed(() => median(successes.value.map((run) => run.total)))
const medianTtfb = computed(() => median(successes.value.map((run) => run.ttfb)))
const slowest = computed(() => Math.max(1, ...results.value.map((run) => run.total)))
const failureCount = computed(() => results.value.length - successes.value.length)

function seconds(ms: number | null): string {
  return ms === null ? '—' : `${(ms / 1000).toFixed(2)}s`
}

/** Pulls the last output token count out of either an SSE stream or a complete body. */
function outputTokens(text: string): number | null {
  const matches = [...text.matchAll(/"output_tokens"\s*:\s*(\d+)/g)]
  return matches.length ? Number(matches[matches.length - 1]![1]) : null
}

async function runOne(index: number): Promise<Run> {
  const started = performance.now()
  const headers: Record<string, string> = { 'content-type': 'application/json' }
  if (apiKey.value.trim()) headers['x-api-key'] = apiKey.value.trim()

  const response = await fetch(`${gatewayBase()}/v1/messages`, {
    method: 'POST',
    headers,
    body: JSON.stringify({
      model: model.value,
      max_tokens: 64,
      stream: stream.value,
      messages: [{ role: 'user', content: 'Reply with exactly: OK' }],
    }),
  })

  if (!response.ok) {
    const text = await response.text()
    return {
      index,
      ok: false,
      status: response.status,
      ttfb: performance.now() - started,
      total: performance.now() - started,
      outputTokens: null,
      error: text.slice(0, 300),
    }
  }

  let ttfb = 0
  let text = ''
  if (stream.value && response.body) {
    const reader = response.body.getReader()
    const decoder = new TextDecoder()
    let first = true
    for (;;) {
      const { done, value } = await reader.read()
      if (done) break
      if (first) {
        ttfb = performance.now() - started
        first = false
      }
      text += decoder.decode(value, { stream: true })
    }
    if (first) ttfb = performance.now() - started
  } else {
    text = await response.text()
    ttfb = performance.now() - started
  }

  return {
    index,
    ok: true,
    status: response.status,
    ttfb,
    total: performance.now() - started,
    outputTokens: outputTokens(text),
  }
}

async function start() {
  if (!model.value) {
    ElMessage.warning('先选一个模型别名')
    return
  }
  running.value = true
  results.value = []
  try {
    for (let index = 1; index <= runs.value; index += 1) {
      try {
        results.value.push(await runOne(index))
      } catch (problem) {
        results.value.push({
          index,
          ok: false,
          status: 0,
          ttfb: 0,
          total: 0,
          outputTokens: null,
          error: problem instanceof Error ? problem.message : String(problem),
        })
      }
    }
  } finally {
    running.value = false
  }
}
</script>

<template>
  <div class="benchmark">
    <el-card shadow="never">
      <template #header><span class="card-title">测试配置</span></template>
      <el-form label-width="96px" label-position="left" size="small" class="form">
        <el-form-item label="模型别名">
          <el-select v-model="model" placeholder="选择一个已配置的别名" class="wide">
            <el-option v-for="alias in aliases" :key="alias" :label="alias" :value="alias" />
          </el-select>
        </el-form-item>
        <el-form-item label="请求次数">
          <el-input-number v-model="runs" :min="1" :max="20" size="small" />
          <span class="gw-hint inline-hint">顺序发送，不并发，测的是单次往返。</span>
        </el-form-item>
        <el-form-item label="流式">
          <el-switch v-model="stream" />
          <span class="gw-hint inline-hint">流式下首字节与总时都会记录。</span>
        </el-form-item>
        <el-form-item label="入站密钥">
          <el-input
            v-model="apiKey"
            type="password"
            show-password
            placeholder="网关未配置入站密钥时留空"
          />
        </el-form-item>
        <el-form-item label=" ">
          <el-button type="primary" :icon="VideoPlay" :loading="running" @click="start">
            开始测试
          </el-button>
        </el-form-item>
      </el-form>
    </el-card>

    <el-card shadow="never">
      <template #header><span class="card-title">结果</span></template>
      <div v-if="!results.length" class="gw-empty">
        还没有结果。选好别名后点「开始测试」。
      </div>
      <template v-else>
        <div class="readout">
          <div class="metric">
            <span class="gw-micro">成功</span>
            <span class="value mono" :class="{ bad: failureCount > 0 }">
              {{ successes.length }}/{{ results.length }}
            </span>
          </div>
          <div class="metric">
            <span class="gw-micro">首字节中位</span>
            <span class="value mono">{{ seconds(medianTtfb) }}</span>
          </div>
          <div class="metric">
            <span class="gw-micro">总时中位</span>
            <span class="value mono">{{ seconds(medianTotal) }}</span>
          </div>
          <div class="metric">
            <span class="gw-micro">最慢一次</span>
            <span class="value mono">{{ seconds(slowest) }}</span>
          </div>
        </div>

        <el-table :data="results" size="small">
          <el-table-column prop="index" label="#" width="52" />
          <el-table-column label="状态" width="88">
            <template #default="{ row }">
              <span class="status" :class="row.ok ? 'ok' : 'bad'">
                <i class="dot" />{{ row.ok ? row.status : row.status || 'FAIL' }}
              </span>
            </template>
          </el-table-column>
          <el-table-column label="首字节" width="96" align="right">
            <template #default="{ row }">
              <span class="mono">{{ row.ok ? seconds(row.ttfb) : '—' }}</span>
            </template>
          </el-table-column>
          <el-table-column label="总时" width="96" align="right">
            <template #default="{ row }">
              <span class="mono">{{ seconds(row.total) }}</span>
            </template>
          </el-table-column>
          <el-table-column label="输出 tokens" width="110" align="right">
            <template #default="{ row }">
              <span class="mono">{{ row.outputTokens ?? '—' }}</span>
            </template>
          </el-table-column>
          <el-table-column label="耗时分布" min-width="220">
            <template #default="{ row }">
              <div class="bar-track">
                <div
                  class="bar"
                  :class="{ bad: !row.ok }"
                  :style="{ width: `${Math.max(2, (row.total / slowest) * 100)}%` }"
                />
              </div>
            </template>
          </el-table-column>
          <el-table-column type="expand">
            <template #default="{ row }">
              <div class="detail">
                <div v-if="row.error" class="mono error-text">{{ row.error }}</div>
                <div v-else class="gw-hint">首字节 {{ seconds(row.ttfb) }} · 总时 {{ seconds(row.total) }}</div>
              </div>
            </template>
          </el-table-column>
        </el-table>
      </template>
    </el-card>
  </div>
</template>

<style scoped>
.benchmark {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.card-title {
  font-weight: 600;
}

.form {
  max-width: 640px;
}

.wide {
  width: 100%;
}

.inline-hint {
  margin-left: 12px;
}

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
}

.metric + .metric {
  border-left: 1px solid var(--gw-line);
}

.value {
  font-size: 19px;
  font-weight: 500;
  line-height: 1.15;
}

.value.bad {
  color: var(--gw-danger);
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

.bar-track {
  height: 8px;
  background: var(--gw-canvas);
  border-radius: 4px;
  overflow: hidden;
}

.bar {
  height: 100%;
  background: var(--gw-accent);
  border-radius: 4px;
  transition: width 0.25s ease;
}

.bar.bad {
  background: var(--gw-danger);
}

.detail {
  padding: 10px 16px;
}

.error-text {
  color: var(--gw-danger);
  word-break: break-all;
}
</style>
