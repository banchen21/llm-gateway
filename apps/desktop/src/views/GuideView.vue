<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { CopyDocument, Delete, Plus } from '@element-plus/icons-vue'
import { useGateway } from '../store'
import { fetchHealth, type Health } from '../api/client'
import {
  endpointsFor,
  PROTOCOL_MODE_LABELS,
  type ApiKey,
} from '../api/types'

const gateway = useGateway()
const { config, aliases } = gateway
const health = ref<Health | null>(null)

/** `0.0.0.0` and `::` are listen addresses, not ones a client can dial. */
const host = computed(() => {
  const bind = config.server.bind.trim()
  return bind === '0.0.0.0' || bind === '::' || bind === '[::]' ? '127.0.0.1' : bind
})
const baseUrl = computed(() => `http://${host.value}:${config.server.port}`)
const liveMode = computed(() => health.value?.protocol_mode ?? config.protocol_mode)
const liveEndpoints = computed(() => endpointsFor(liveMode.value))

/** A key to paste into the samples; the first one the user actually created wins. */
const sampleKey = computed(
  () => config.server.api_keys.find((entry) => entry.key.trim())?.key
    ?? config.server.api_key
    ?? '<你的密钥>',
)
const sampleModel = computed(() => aliases.value[0] ?? '<模型别名>')

const endpointRows = computed(() =>
  liveEndpoints.value.map((path) => ({
    path,
    method: 'POST',
    protocol: path.startsWith('/v1/chat') ? 'OpenAI' : 'Anthropic',
  })),
)

const servesOpenAi = computed(() => liveEndpoints.value.includes('/v1/chat/completions'))

const anthropicSample = computed(() => `curl ${baseUrl.value}/v1/messages \\
  -H 'content-type: application/json' \\
  -H 'x-api-key: ${sampleKey.value}' \\
  -d '{"model":"${sampleModel.value}","max_tokens":256,"messages":[{"role":"user","content":"你好"}]}'`)

const openAiSample = computed(() => `curl ${baseUrl.value}/v1/chat/completions \\
  -H 'content-type: application/json' \\
  -H 'Authorization: Bearer ${sampleKey.value}' \\
  -d '{"model":"${sampleModel.value}","messages":[{"role":"user","content":"你好"}]}'`)

const claudeCodeSample = computed(() => `ANTHROPIC_BASE_URL=${baseUrl.value}
ANTHROPIC_AUTH_TOKEN=${sampleKey.value}
ANTHROPIC_MODEL=${sampleModel.value}`)

onMounted(refresh)

async function refresh() {
  try {
    health.value = await fetchHealth()
  } catch {
    health.value = null
  }
}

/** 24 random bytes, hex encoded, prefixed so it is recognisable in a log. */
function generateKey(): string {
  const bytes = new Uint8Array(24)
  crypto.getRandomValues(bytes)
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('')
  return `sk-gw-${hex}`
}

function addKey() {
  const entry: ApiKey = {
    id: '',
    name: `客户端 ${config.server.api_keys.length + 1}`,
    key: generateKey(),
  }
  config.server.api_keys.push(entry)
}

async function removeKey(index: number) {
  const entry = config.server.api_keys[index]
  if (!entry) return
  try {
    await ElMessageBox.confirm(
      `删除「${entry.name || '未命名'}」之后，使用这个密钥的客户端会立即失去访问权限。`,
      '删除密钥',
      { type: 'warning', confirmButtonText: '删除', cancelButtonText: '取消' },
    )
  } catch {
    return
  }
  config.server.api_keys.splice(index, 1)
}

function mask(key: string): string {
  if (!key) return '（空）'
  if (key.length <= 14) return key
  return `${key.slice(0, 10)}…${key.slice(-4)}`
}

async function copy(text: string) {
  try {
    await navigator.clipboard.writeText(text)
    ElMessage.success('已复制')
  } catch {
    ElMessage.warning('浏览器拒绝了剪贴板访问，请手动复制')
  }
}
</script>

<template>
  <div class="guide">
    <el-card shadow="never">
      <template #header><span class="card-title">服务地址</span></template>
      <div class="gw-section">
        <div class="gw-section-title">地址</div>
        <div class="gw-section-note">任何兼容 Anthropic 或 OpenAI 协议的客户端都可以连这里。</div>
        <div class="grow" />
      </div>
      <div class="access">
        <span class="mono">{{ baseUrl }}</span>
        <el-button text size="small" :icon="CopyDocument" @click="copy(baseUrl)">复制</el-button>
      </div>
      <div class="gw-hint">
        修改监听地址或端口只能在重启网关后生效；当前生效的协议模式是
        {{ PROTOCOL_MODE_LABELS[liveMode] }}。
      </div>
    </el-card>

    <el-card shadow="never">
      <template #header><span class="card-title">接入接口</span></template>
      <el-table :data="endpointRows" size="small" border>
        <el-table-column prop="protocol" label="协议" width="120" />
        <el-table-column prop="method" label="方法" width="90" />
        <el-table-column prop="path" label="路径" min-width="240">
          <template #default="{ row }"><span class="mono">{{ row.path }}</span></template>
        </el-table-column>
      </el-table>
      <div class="gw-hint">
        认证可以用 <span class="mono">x-api-key</span> 或
        <span class="mono">Authorization: Bearer</span>；两者取其一即可。
        <span class="mono">/v1/models</span> 始终可用，返回全部模型别名。
        未配置密钥时不校验。
      </div>
    </el-card>

    <el-card shadow="never">
      <template #header><span class="card-title">可用模型</span></template>
      <div v-if="aliases.length" class="aliases">
        <el-tag v-for="alias in aliases" :key="alias" size="small" effect="plain" class="alias">
          {{ alias }}
        </el-tag>
      </div>
      <div v-else class="gw-empty">还没有配置任何模型别名，先去「供应商」里添加。</div>
      <div class="gw-hint">
        客户端请求时填这些别名，网关会把它映射到供应商自己的上游模型名。
      </div>
    </el-card>

    <el-card shadow="never">
      <template #header><span class="card-title">密钥</span></template>
      <div class="gw-section">
        <div class="gw-section-title">入站密钥</div>
        <div class="gw-section-note">每个客户端一个密钥，吊销其中一个不会影响其他客户端。</div>
        <div class="grow" />
        <el-button size="small" type="primary" :icon="Plus" @click="addKey">生成密钥</el-button>
      </div>
      <el-table v-if="config.server.api_keys.length" :data="config.server.api_keys" size="small" border>
        <el-table-column label="用途" min-width="180">
          <template #default="{ row }">
            <el-input v-model="row.name" size="small" placeholder="例如 Claude Desktop" />
          </template>
        </el-table-column>
        <el-table-column label="密钥" min-width="260">
          <template #default="{ row }"><span class="mono">{{ mask(row.key) }}</span></template>
        </el-table-column>
        <el-table-column label="操作" width="160">
          <template #default="{ row, $index }">
            <el-button text size="small" :icon="CopyDocument" @click="copy(row.key)">复制</el-button>
            <el-button text size="small" type="danger" :icon="Delete" @click="removeKey($index)">
              删除
            </el-button>
          </template>
        </el-table-column>
      </el-table>
      <div v-else class="gw-empty">还没有密钥。没有密钥时任何调用方都能使用网关。</div>
      <div class="gw-hint">
        新生成的密钥显示为掩码，点「复制」可以拿到完整值。改动需要点右上角「保存并生效」。
      </div>
      <div v-if="config.server.api_key" class="gw-hint">
        另外还有一个旧版单密钥（在「模式与端口」里维护），它同样有效。
      </div>
    </el-card>

    <el-card shadow="never">
      <template #header><span class="card-title">接入示例</span></template>
      <div class="gw-section"><div class="gw-section-title">Anthropic 协议</div></div>
      <pre class="code">{{ anthropicSample }}</pre>

      <template v-if="servesOpenAi">
        <div class="gw-section"><div class="gw-section-title">OpenAI 协议</div></div>
        <pre class="code">{{ openAiSample }}</pre>
      </template>
      <div v-else class="gw-hint">
        当前模式不开放 <span class="mono">/v1/chat/completions</span>；需要时在「模式与端口」
        里切换为路由协议。
      </div>

      <div class="gw-section"><div class="gw-section-title">Claude Code / Claude Desktop（第三方推理）</div></div>
      <pre class="code">{{ claudeCodeSample }}</pre>
    </el-card>
  </div>
</template>

<style scoped>
.guide {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.grow {
  flex: 1;
}

.card-title {
  font-weight: 600;
}

.access {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  background: var(--gw-canvas);
  border-radius: 6px;
}

.aliases {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}

.alias {
  font-family: var(--gw-mono);
}

.code {
  margin: 8px 0 0;
  padding: 12px;
  background: var(--gw-canvas);
  border: 1px solid var(--gw-line);
  border-radius: 6px;
  font-family: var(--gw-mono);
  font-size: 12px;
  line-height: 1.6;
  white-space: pre-wrap;
  word-break: break-all;
}
</style>
