<script setup lang="ts">
import { computed, reactive, ref } from 'vue'
import { ElMessageBox } from 'element-plus'
import { Delete, Plus } from '@element-plus/icons-vue'
import { useGateway } from '../store'
import {
  emptyModel,
  emptyProvider,
  type Provider,
  type ProviderModel,
  type ProviderProtocol,
  type MaxTokensField,
} from '../api/types'

const gateway = useGateway()
const { config } = gateway

/** Which provider cards are open, so the list stays readable as it grows. */
const expanded = ref<string[]>([])

/**
 * Extra parameters are edited in a dialog rather than inline.
 *
 * Inline editing cannot work here: the textarea would have to round-trip through the
 * parsed object on every keystroke, so a half-typed `{` would be erased before the user
 * could finish it.
 */
const extraDialog = reactive({
  open: false,
  providerName: '',
  modelAlias: '',
  text: '',
  error: '',
  target: null as ProviderModel | null,
})

const duplicateAlias = computed(() => {
  const seen = new Map<string, number>()
  for (const provider of config.providers) {
    if (!provider.enabled) continue
    for (const model of provider.models) {
      if (!model.enabled) continue
      const alias = model.alias.trim()
      if (alias) seen.set(alias, (seen.get(alias) ?? 0) + 1)
    }
  }
  return new Set([...seen.entries()].filter(([, count]) => count > 1).map(([alias]) => alias))
})

function addProvider() {
  config.providers.push(emptyProvider())
  expanded.value = [String(config.providers.length - 1), ...expanded.value]
}

async function removeProvider(index: number) {
  const provider = config.providers[index]!
  await ElMessageBox.confirm(
    `删除供应商「${provider.name || '未命名'}」及其 ${provider.models.length} 个模型配置？`,
    '确认删除',
    { confirmButtonText: '删除', cancelButtonText: '取消', type: 'warning' },
  )
  if (provider.id && config.default_provider_id === provider.id) {
    config.default_provider_id = null
  }
  config.providers.splice(index, 1)
}

function headerEntries(provider: Provider) {
  return Object.entries(provider.custom_headers)
}

/**
 * Renames or updates a header. Renaming rebuilds the record, so it is committed on blur
 * rather than per keystroke — an index-keyed row would otherwise be recreated mid-typing.
 */
function setHeader(provider: Provider, previous: string, next: string, value?: string) {
  const rebuilt: Record<string, string> = {}
  for (const [key, current] of Object.entries(provider.custom_headers)) {
    if (key === previous) {
      const name = next.trim() || previous
      rebuilt[name] = value ?? current
    } else {
      rebuilt[key] = current
    }
  }
  provider.custom_headers = rebuilt
}

function addHeader(provider: Provider) {
  let name = 'x-header'
  let suffix = 1
  while (name in provider.custom_headers) {
    name = `x-header-${++suffix}`
  }
  provider.custom_headers = { ...provider.custom_headers, [name]: '' }
}

function removeHeader(provider: Provider, key: string) {
  const { [key]: _removed, ...rest } = provider.custom_headers
  provider.custom_headers = rest
}

function openExtra(provider: Provider, model: ProviderModel) {
  extraDialog.open = true
  extraDialog.providerName = provider.name || '未命名供应商'
  extraDialog.modelAlias = model.alias || '未命名模型'
  extraDialog.text = Object.keys(model.extra).length ? JSON.stringify(model.extra, null, 2) : ''
  extraDialog.error = ''
  extraDialog.target = model
}

function confirmExtra() {
  const model = extraDialog.target
  if (!model) return
  if (!extraDialog.text.trim()) {
    model.extra = {}
    extraDialog.open = false
    return
  }
  let parsed: unknown
  try {
    parsed = JSON.parse(extraDialog.text)
  } catch (error) {
    extraDialog.error = error instanceof Error ? error.message : '不是合法的 JSON'
    return
  }
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
    extraDialog.error = '必须是一个 JSON 对象'
    return
  }
  model.extra = parsed as Record<string, unknown>
  extraDialog.open = false
}

function extraSummary(model: ProviderModel): string {
  const keys = Object.keys(model.extra)
  return keys.length ? `${keys.length} 项` : '—'
}

const baseUrlHint: Record<ProviderProtocol, string> = {
  anthropic: '例如 https://api.anthropic.com，不要带 /v1/messages',
  open_ai: '例如 https://api.openai.com，不要带 /v1/chat/completions',
}

const tokensFields: MaxTokensField[] = ['max_tokens', 'max_completion_tokens']
</script>

<template>
  <div>
    <div class="gw-section">
      <span class="gw-section-title">供应商</span>
      <span class="gw-section-note">
        上游可以是 Anthropic 兼容端点，也可以是 OpenAI 兼容端点。协议模式只决定网关对外开放哪些接口，与这里无关。
      </span>
      <div class="grow" />
      <el-button type="primary" :icon="Plus" size="small" @click="addProvider">
        添加供应商
      </el-button>
    </div>

    <div v-if="!config.providers.length" class="gw-empty">
      还没有供应商。点击「添加供应商」接入第一个上游端点。
    </div>

    <el-card
      v-for="(provider, index) in config.providers"
      :key="provider.id || `new-${index}`"
      shadow="never"
      class="provider"
    >
      <template #header>
        <div class="provider-head">
          <el-input
            v-model="provider.name"
            placeholder="供应商名称"
            size="small"
            class="provider-name"
          />
          <el-tag size="small" effect="plain">
            {{ provider.protocol === 'anthropic' ? 'Anthropic 协议' : 'OpenAI 协议' }}
          </el-tag>
          <el-switch v-model="provider.enabled" size="small" />
          <span class="gw-hint">{{ provider.enabled ? '启用' : '已停用' }}</span>
          <div class="grow" />
          <el-button text size="small" type="danger" :icon="Delete" @click="removeProvider(index)">
            删除
          </el-button>
        </div>
      </template>

      <el-form label-width="112px" label-position="left" size="small">
        <el-form-item label="协议">
          <el-radio-group v-model="provider.protocol">
            <el-radio value="anthropic">Anthropic</el-radio>
            <el-radio value="open_ai">OpenAI</el-radio>
          </el-radio-group>
        </el-form-item>

        <el-form-item label="Base URL">
          <el-input v-model="provider.base_url" :placeholder="baseUrlHint[provider.protocol]" />
        </el-form-item>

        <el-form-item label="API Key">
          <el-input v-model="provider.api_key" type="password" show-password placeholder="上游密钥" />
        </el-form-item>

        <el-form-item label="备用密钥">
          <div class="keys">
            <div v-for="(entry, keyIndex) in provider.api_keys" :key="keyIndex" class="key-row">
              <el-input v-model="entry.name" size="small" placeholder="用途（可选）" />
              <el-input v-model="entry.key" size="small" type="password" show-password placeholder="密钥" />
              <el-button
                text
                size="small"
                type="danger"
                :icon="Delete"
                @click="provider.api_keys.splice(keyIndex, 1)"
              >
                删除
              </el-button>
            </div>
            <el-button text size="small" :icon="Plus" @click="provider.api_keys.push({ id: '', name: '', key: '' })">
              添加密钥
            </el-button>
            <div class="gw-hint">
              同一个供应商的多个账号轮流使用；某个 key 被限流（429）或被拒（401/403）时自动换下一个。
            </div>
          </div>
        </el-form-item>

        <el-form-item label="自定义请求头">
          <div class="headers">
            <div v-for="([key, value], headerIndex) in headerEntries(provider)" :key="headerIndex" class="header-row">
              <el-input
                :model-value="key"
                size="small"
                placeholder="名称"
                @change="(next: string) => setHeader(provider, key, next)"
              />
              <el-input
                :model-value="value"
                size="small"
                placeholder="值"
                @update:model-value="(next: string) => setHeader(provider, key, key, next)"
              />
              <el-button text size="small" type="danger" @click="removeHeader(provider, key)">
                移除
              </el-button>
            </div>
            <el-button text size="small" :icon="Plus" @click="addHeader(provider)">
              添加请求头
            </el-button>
          </div>
        </el-form-item>
      </el-form>

      <div class="gw-section models-head">
        <span class="gw-section-title">模型</span>
        <span class="gw-section-note">
          别名是客户端请求时使用的 model 名；上游模型名是这家供应商自己的叫法。
        </span>
        <div class="grow" />
        <el-button size="small" :icon="Plus" @click="provider.models.push(emptyModel())">
          添加模型
        </el-button>
      </div>

      <el-table v-if="provider.models.length" :data="provider.models" size="small" border>
        <el-table-column label="别名" min-width="190">
          <template #default="{ row }">
            <el-input v-model="row.alias" size="small" placeholder="客户端请求的 model" />
            <div v-if="duplicateAlias.has(row.alias.trim())" class="alias-conflict">
              该别名与其他启用的模型重复，保存时会被拒绝
            </div>
          </template>
        </el-table-column>

        <el-table-column label="上游模型名" min-width="190">
          <template #default="{ row }">
            <el-input v-model="row.upstream_model" size="small" placeholder="供应商自己的模型 id" />
          </template>
        </el-table-column>

        <el-table-column label="输出上限" width="152">
          <template #default="{ row }">
            <el-input-number
              v-model="row.max_output_tokens"
              :min="1"
              :max="1000000"
              :controls="false"
              size="small"
              placeholder="不限"
              class="full"
            />
          </template>
        </el-table-column>

        <el-table-column v-if="provider.protocol === 'open_ai'" label="计数字段" width="176">
          <template #default="{ row }">
            <el-select v-model="row.max_tokens_field" size="small">
              <el-option v-for="field in tokensFields" :key="field" :label="field" :value="field" />
            </el-select>
          </template>
        </el-table-column>

        <el-table-column label="额外参数" width="112" align="center">
          <template #default="{ row }">
            <el-button text size="small" @click="openExtra(provider, row)">
              {{ extraSummary(row) }}
            </el-button>
          </template>
        </el-table-column>

        <el-table-column label="启用" width="72" align="center">
          <template #default="{ row }">
            <el-switch v-model="row.enabled" size="small" />
          </template>
        </el-table-column>

        <el-table-column label="操作" width="72" align="center">
          <template #default="{ $index }">
            <el-button text size="small" type="danger" @click="provider.models.splice($index, 1)">
              删除
            </el-button>
          </template>
        </el-table-column>
      </el-table>

      <div v-else class="gw-hint empty-models">
        还没有模型，客户端无法通过这个供应商请求任何模型。
      </div>
    </el-card>

    <el-dialog v-model="extraDialog.open" title="额外参数" width="560px">
      <p class="gw-hint dialog-lead">
        {{ extraDialog.providerName }} · {{ extraDialog.modelAlias }}
        <br />
        下面这段 JSON 会原样并入上游请求体，用于网关未建模的厂商参数。它不能覆盖 model、messages、
        tools、stream 这类语义字段 —— 那样会被网关拒绝。
      </p>
      <el-input v-model="extraDialog.text" type="textarea" :rows="10" placeholder='{ "top_k": 40 }' />
      <el-alert
        v-if="extraDialog.error"
        type="error"
        show-icon
        :closable="false"
        :title="extraDialog.error"
        class="dialog-error"
      />
      <template #footer>
        <el-button size="small" @click="extraDialog.open = false">取消</el-button>
        <el-button size="small" type="primary" @click="confirmExtra">确定</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.grow {
  flex: 1;
}

.provider {
  margin-bottom: 16px;
}

.provider-head {
  display: flex;
  align-items: center;
  gap: 10px;
}

.provider-name {
  width: 260px;
}

.headers {
  display: flex;
  flex-direction: column;
  gap: 8px;
  width: 100%;
}

.header-row {
  display: grid;
  grid-template-columns: 220px 1fr auto;
  gap: 8px;
}

.keys {
  display: flex;
  flex-direction: column;
  gap: 8px;
  width: 100%;
}

.key-row {
  display: grid;
  grid-template-columns: 180px 1fr auto;
  gap: 8px;
}

.models-head {
  margin-top: 20px;
}

.full {
  width: 100%;
}

.alias-conflict {
  margin-top: 2px;
  color: var(--el-color-danger);
  font-size: 11px;
  line-height: 1.3;
}

.empty-models {
  padding: 12px 0;
}

.dialog-lead {
  margin: 0 0 12px;
}

.dialog-error {
  margin-top: 12px;
}
</style>
