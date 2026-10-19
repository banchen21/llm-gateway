<script setup lang="ts">
import { computed, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { VideoPlay } from '@element-plus/icons-vue'
import { gatewayBase } from '../api/client'
import { useGateway } from '../store'

const gateway = useGateway()
const { aliases } = gateway

/**
 * One fixed prompt for every alias: the same line, the same number of times.
 *
 * Comparing throughput only means something when every run produces the same output, so
 * the instruction pins both the content and the length. The token count actually reported
 * by the upstream is what the rate is computed from, so a model that ignores the
 * instruction still yields an honest number for what it produced.
 */
const PROMPT = `Repeat the following line exactly 20 times, one per line, with no numbering, no explanation, and nothing else:

The quick brown fox jumps over the lazy dog.`

interface Run {
  index: number
  ok: boolean
  status: number
  ttfb: number
  total: number
  outputTokens: number | null
  error?: string
}

interface AliasResult {
  alias: string
  runs: Run[]
}

const runsPerAlias = ref(1)
const apiKey = ref('')
const running = ref(false)
const results = ref<AliasResult[]>([])
const completed = ref(0)

function median(values: number[]): number | null {
  if (!values.length) return null
  const sorted = [...values].sort((a, b) => a - b)
  const middle = Math.floor(sorted.length / 2)
  return sorted.length % 2
    ? sorted[middle]!
    : (sorted[middle - 1]! + sorted[middle]!) / 2
}

function seconds(ms: number | null): string {
  return ms === null ? '—' : `${(ms / 1000).toFixed(2)}s`
}

/** Tokens per second after the first byte, which is what "rate" means here. */
function rate(run: Run): number | null {
  if (!run.ok || !run.outputTokens) return null
  const generating = (run.total - run.ttfb) / 1000
  return generating > 0 ? run.outputTokens / generating : null
}

function formatRate(value: number | null): string {
  return value === null ? '—' : `${value.toFixed(1)}/s`
}

function summary(entry: AliasResult) {
  const ok = entry.runs.filter((run) => run.ok)
  return {
    successes: ok.length,
    total: entry.runs.length,
    ttfb: median(ok.map((run) => run.ttfb)),
    duration: median(ok.map((run) => run.total)),
    tokens: median(ok.map((run) => run.outputTokens ?? 0)),
    rate: median(ok.map(rate).filter((value): value is number => value !== null)),
  }
}

const summaries = computed(() => results.value.map((entry) => ({ entry, ...summary(entry) })))

/** Pulls the last output token count out of the SSE stream. */
function outputTokens(text: string): number | null {
  const matches = [...text.matchAll(/"output_tokens"\s*:\s*(\d+)/g)]
  return matches.length ? Number(matches[matches.length - 1]![1]) : null
}

async function runOne(alias: string, index: number): Promise<Run> {
  const started = performance.now()
  const headers: Record<string, string> = { 'content-type': 'application/json' }
  if (apiKey.value.trim()) headers['x-api-key'] = apiKey.value.trim()

  const response = await fetch(`${gatewayBase()}/v1/messages`, {
    method: 'POST',
    headers,
    body: JSON.stringify({
      model: alias,
      max_tokens: 512,
      stream: true,
      messages: [{ role: 'user', content: PROMPT }],
    }),
  })

  if (!response.ok) {
    const text = await response.text()
    const at = performance.now() - started
    return { index, ok: false, status: response.status, ttfb: at, total: at, outputTokens: null, error: text.slice(0, 300) }
  }

  let ttfb = 0
  let text = ''
  if (response.body) {
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
  if (!aliases.value.length) {
    ElMessage.warning('还没有配置任何模型别名')
    return
  }
  running.value = true
  completed.value = 0
  results.value = aliases.value.map((alias) => ({ alias, runs: [] }))
  try {
    // Every alias goes out at once: comparing them one after another would let a slow
    // first alias delay the rest and make the total run far longer than the test itself.
    await Promise.all(
      results.value.map(async (entry) => {
        for (let index = 1; index <= runsPerAlias.value; index += 1) {
          try {
            entry.runs.push(await runOne(entry.alias, index))
          } catch (problem) {
            entry.runs.push({
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
        completed.value += 1
      }),
    )
  } finally {
    running.value = false
  }
}
</script>

<template>
  <div class="benchmark">
    <el-card shadow="never">
      <template #header><span class="card-title">测试配置</span></template>
      <el-form label-width="112px" label-position="left" size="small" class="form">
        <el-form-item label="测试对象">
          <span class="gw-hint">全部 {{ aliases.length }} 个别名，同时发送。</span>
        </el-form-item>
        <el-form-item label="每个别名次数">
          <el-input-number v-model="runsPerAlias" :min="1" :max="5" size="small" />
          <span class="gw-hint inline-hint">同一个别名的多次请求顺序发送。</span>
        </el-form-item>
        <el-form-item label="入站密钥">
          <el-input
            v-model="apiKey"
            type="password"
            show-password
            placeholder="网关未配置入站密钥时留空"
          />
        </el-form-item>
        <el-form-item label="固定提示词">
          <pre class="prompt">{{ PROMPT }}</pre>
        </el-form-item>
        <el-form-item label=" ">
          <el-button type="primary" :icon="VideoPlay" :loading="running" @click="start">
            开始测试
          </el-button>
          <span v-if="running" class="gw-hint inline-hint">
            已完成 {{ completed }}/{{ results.length }} 个别名…
          </span>
        </el-form-item>
      </el-form>
    </el-card>

    <el-card shadow="never">
      <template #header><span class="card-title">结果</span></template>
      <div v-if="!results.length" class="gw-empty">
        还没有结果。点击「开始测试」会依次测完所有别名。
      </div>
      <el-table v-else :data="summaries" size="small">
        <el-table-column prop="entry.alias" label="模型别名" min-width="170">
          <template #default="{ row }"><span class="mono">{{ row.entry.alias }}</span></template>
        </el-table-column>
        <el-table-column label="成功" width="80">
          <template #default="{ row }">
            <span class="status" :class="row.successes === row.total ? 'ok' : 'bad'">
              <i class="dot" />{{ row.successes }}/{{ row.total }}
            </span>
          </template>
        </el-table-column>
        <el-table-column label="延迟（首字节）" width="132" align="right">
          <template #default="{ row }"><span class="mono">{{ seconds(row.ttfb) }}</span></template>
        </el-table-column>
        <el-table-column label="耗时（总计）" width="126" align="right">
          <template #default="{ row }"><span class="mono">{{ seconds(row.duration) }}</span></template>
        </el-table-column>
        <el-table-column label="输出 tokens" width="112" align="right">
          <template #default="{ row }"><span class="mono">{{ row.tokens ?? '—' }}</span></template>
        </el-table-column>
        <el-table-column label="速率" width="110" align="right">
          <template #default="{ row }"><span class="mono strong">{{ formatRate(row.rate) }}</span></template>
        </el-table-column>
        <el-table-column type="expand">
          <template #default="{ row }">
            <div class="runs">
              <div v-for="run in row.entry.runs" :key="run.index" class="run">
                <span class="gw-micro">第 {{ run.index }} 次</span>
                <span v-if="run.ok" class="mono">
                  首字节 {{ seconds(run.ttfb) }} · 总计 {{ seconds(run.total) }} ·
                  {{ run.outputTokens ?? '—' }} tokens · {{ formatRate(rate(run)) }}
                </span>
                <span v-else class="mono error-text">
                  {{ run.status || 'FAIL' }} · {{ run.error }}
                </span>
              </div>
            </div>
          </template>
        </el-table-column>
      </el-table>
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
  max-width: 720px;
}

.inline-hint {
  margin-left: 12px;
}

.prompt {
  margin: 0;
  padding: 10px 12px;
  background: var(--gw-canvas);
  border: 1px solid var(--gw-line);
  border-radius: 6px;
  font-family: var(--gw-mono);
  font-size: 12px;
  line-height: 1.6;
  white-space: pre-wrap;
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

.strong {
  font-weight: 600;
}

.runs {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 10px 16px;
}

.run {
  display: flex;
  align-items: baseline;
  gap: 14px;
}

.error-text {
  color: var(--gw-danger);
  word-break: break-all;
}
</style>
