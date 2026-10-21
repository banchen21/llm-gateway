<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Delete, Download, Plus, Refresh } from '@element-plus/icons-vue'
import { fetchProviderModels, fetchQuotas, refreshProviderQuota } from '../api/client'
import HeaderRow from '../components/HeaderRow.vue'
import { useGateway } from '../store'
import {
  emptyModel,
  emptyProvider,
  type KeyQuota,
  type Provider,
  type ProviderModel,
  type ProviderProtocol,
  type ProviderQuota,
  type MaxTokensField,
  type QuotaWindow,
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

/** The upstream model picker, shared by every provider card. */
const modelDialog = reactive({
  open: false,
  loading: false,
  provider: null as Provider | null,
  models: [] as string[],
  picked: [] as string[],
})

async function openModelFetch(provider: Provider) {
  modelDialog.provider = provider
  modelDialog.models = []
  modelDialog.picked = []
  modelDialog.open = true
  modelDialog.loading = true
  try {
    const { models } = await fetchProviderModels(provider)
    modelDialog.models = models
    // Models the provider already lists stay unchecked; fetching again is additive.
    const known = new Set(provider.models.map((model) => model.upstream_model))
    modelDialog.picked = models.filter((id) => !known.has(id))
  } catch (problem) {
    modelDialog.open = false
    ElMessage.error(problem instanceof Error ? problem.message : String(problem))
  } finally {
    modelDialog.loading = false
  }
}

function addPickedModels() {
  const provider = modelDialog.provider
  if (!provider) return
  for (const id of modelDialog.picked) {
    const model = emptyModel()
    model.alias = id
    model.upstream_model = id
    provider.models.push(model)
  }
  ElMessage.success(`已添加 ${modelDialog.picked.length} 个模型`)
  modelDialog.open = false
}

function headerEntries(provider: Provider) {
  return Object.entries(provider.custom_headers)
}

/** Names a header may not take, because another row already holds one. */
function headerNames(provider: Provider, except: string): string[] {
  return Object.keys(provider.custom_headers).filter((name) => name !== except)
}

function renameHeader(provider: Provider, previous: string, next: string) {
  if (next === previous) return
  const rebuilt: Record<string, string> = {}
  for (const [name, current] of Object.entries(provider.custom_headers)) {
    rebuilt[name === previous ? next : name] = current
  }
  provider.custom_headers = rebuilt
}

function setHeaderValue(provider: Provider, name: string, value: string) {
  provider.custom_headers = { ...provider.custom_headers, [name]: value }
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

/**
 * Account quotas, keyed by provider id; an unsaved draft uses its base URL, which is
 * also what the gateway caches it under.
 */
const quotas = ref<Record<string, ProviderQuota>>({})
const quotaLoading = ref<Record<string, boolean>>({})
const quotaError = ref<Record<string, string>>({})

function providerKey(provider: Provider): string {
  return provider.id || provider.base_url.trim()
}

function isCommandCode(baseUrl: string): boolean {
  try {
    const host = new URL(baseUrl).hostname
    return host === 'commandcode.ai' || host.endsWith('.commandcode.ai')
  } catch {
    return false
  }
}

function quotaFor(provider: Provider): ProviderQuota | undefined {
  // A just-saved provider gains an id, but the reading taken while it was a draft is
  // still filed under its base URL.
  const quota =
    quotas.value[providerKey(provider)] ?? quotas.value[provider.base_url.trim()]
  return quota?.supported ? quota : undefined
}

/** Picks up readings the background refresh has already made, without a network call. */
async function loadQuotas() {
  try {
    const { providers } = await fetchQuotas()
    // Merge rather than replace: a draft's manual reading is not in the gateway's cache
    // and must survive the periodic re-read.
    const next: Record<string, ProviderQuota> = { ...quotas.value }
    for (const quota of providers) {
      next[quota.provider_id] = quota
    }
    quotas.value = next
  } catch {
    // The status panel simply stays empty until the gateway answers.
  }
}

let quotaTimer: ReturnType<typeof setInterval> | undefined

onMounted(() => {
  void loadQuotas()
  // The gateway refreshes in the background; this only re-reads its cache.
  quotaTimer = setInterval(() => void loadQuotas(), 60_000)
})

onUnmounted(() => {
  if (quotaTimer) clearInterval(quotaTimer)
})

async function refreshQuota(provider: Provider) {
  const key = providerKey(provider)
  if (!key) {
    ElMessage.warning('请先填写 Base URL')
    return
  }
  quotaLoading.value = { ...quotaLoading.value, [key]: true }
  quotaError.value = { ...quotaError.value, [key]: '' }
  try {
    const result = await refreshProviderQuota(provider)
    if (!result.supported || !result.quota) {
      ElMessage.info('这个端点没有网关支持的额度接口')
      return
    }
    // Keep both names: a draft is cached by base URL, the saved provider by id.
    quotas.value = {
      ...quotas.value,
      [key]: result.quota,
      [result.quota.provider_id]: result.quota,
    }
  } catch (problem) {
    quotaError.value = {
      ...quotaError.value,
      [key]: problem instanceof Error ? problem.message : String(problem),
    }
  } finally {
    quotaLoading.value = { ...quotaLoading.value, [key]: false }
  }
}

function creditsOf(entry: KeyQuota): number | null {
  const values = [entry.monthly_credits, entry.purchased_credits, entry.free_credits].filter(
    (value): value is number => value !== null,
  )
  return values.length ? values.reduce((sum, value) => sum + value, 0) : null
}

/**
 * The same cutoff the gateway uses: CommandCode answers "insufficient credits" below
 * roughly one credit, even for a small request.
 */
const MIN_USABLE_CREDITS = 1.0

function creditsText(entry: KeyQuota): string {
  const credits = creditsOf(entry)
  return credits === null ? '—' : credits.toFixed(2)
}

function windowText(window: QuotaWindow | null): string {
  return window ? `${window.used.toFixed(2)} / ${window.cap.toFixed(0)}` : '—'
}

function resetText(ms: number | null): string {
  if (!ms) return ''
  return new Date(ms).toLocaleString('zh-CN', {
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
  })
}

function updatedText(quota: ProviderQuota): string {
  return new Date(quota.fetched_at).toLocaleTimeString('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
  })
}

type TagType = 'success' | 'warning' | 'danger' | 'info' | 'primary'

/**
 * The account the quota data says to use first: known credits, no closed window, and the
 * most credit left. Runtime refusals can move it down for a minute; the tag describes the
 * quota-based preference, which is what the panel is showing.
 */
function isPreferred(provider: Provider, entry: KeyQuota): boolean {
  const keys = quotaFor(provider)?.keys ?? []
  let best: KeyQuota | null = null
  let bestCredits = -Infinity
  for (const candidate of keys) {
    if (quotaState(candidate).type !== 'success') continue
    const credits = creditsOf(candidate)
    if (credits === null || credits < MIN_USABLE_CREDITS) continue
    if (credits > bestCredits) {
      best = candidate
      bestCredits = credits
    }
  }
  return best === entry
}

/** The same verdict the request path uses, so the table explains what will be chosen. */
function quotaState(entry: KeyQuota): { label: string; type: TagType } {
  if (entry.error) return { label: '读取失败，暂不参与排序', type: 'warning' }
  const now = Date.now()
  const windows = [
    ['5 小时', entry.five_hour],
    ['本周', entry.weekly],
  ] as const
  for (const [name, window] of windows) {
    if (window?.exceeded && (!window.reset_at || window.reset_at > now)) {
      const reset = resetText(window.reset_at)
      return {
        label: reset ? `${name}超限 · ${reset} 重置` : `${name}超限`,
        type: 'danger',
      }
    }
  }
  const credits = creditsOf(entry)
  if (credits !== null && credits < MIN_USABLE_CREDITS) {
    return { label: `额度不足（< ${MIN_USABLE_CREDITS.toFixed(2)}）`, type: 'danger' }
  }
  return { label: '可用', type: 'success' }
}
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
              同一个供应商的多个账号自动轮换；某个 key 被限流（429）或被拒（401/403）时换下一个。
              CommandCode 端点还会按账号额度排序，额度充足且未超限的账号优先。
            </div>
          </div>
        </el-form-item>

        <el-form-item label="自定义请求头">
          <div class="headers">
            <HeaderRow
              v-for="([key, value], headerIndex) in headerEntries(provider)"
              :key="headerIndex"
              :name="key"
              :value="value"
              :taken="headerNames(provider, key)"
              @rename="(next: string) => renameHeader(provider, key, next)"
              @update:value="(next: string) => setHeaderValue(provider, key, next)"
              @remove="removeHeader(provider, key)"
            />
            <el-button text size="small" :icon="Plus" @click="addHeader(provider)">
              添加请求头
            </el-button>
          </div>
        </el-form-item>
      </el-form>

      <div v-if="isCommandCode(provider.base_url)" class="quota">
        <div class="gw-section quota-head">
          <span class="gw-section-title">账号额度</span>
          <span class="gw-section-note">
            后台每 5 分钟自动读取一次；请求优先使用额度充足、未超限的账号，窗口重置后自动恢复。
          </span>
          <div class="grow" />
          <span v-if="quotaFor(provider)" class="gw-hint">
            更新于 {{ updatedText(quotaFor(provider)!) }}
          </span>
          <el-button
            size="small"
            :icon="Refresh"
            :loading="!!quotaLoading[providerKey(provider)]"
            @click="refreshQuota(provider)"
          >
            刷新额度
          </el-button>
        </div>

        <el-alert
          v-if="quotaError[providerKey(provider)]"
          type="error"
          show-icon
          :closable="false"
          :title="quotaError[providerKey(provider)]"
        />

        <el-table
          v-if="quotaFor(provider)?.keys.length"
          :data="quotaFor(provider)!.keys"
          size="small"
          border
        >
          <el-table-column label="账号" min-width="160">
            <template #default="{ row }">
              <div class="quota-account">{{ row.account || row.hint }}</div>
              <div class="gw-hint">{{ row.email || row.hint }}</div>
            </template>
          </el-table-column>

          <el-table-column label="套餐" width="150">
            <template #default="{ row }">
              <div>{{ row.plan || '—' }}</div>
              <div class="gw-hint">{{ row.plan_status || '' }}</div>
            </template>
          </el-table-column>

          <el-table-column label="剩余额度" width="110" align="right">
            <template #default="{ row }">{{ creditsText(row) }}</template>
          </el-table-column>

          <el-table-column label="5 小时用量" width="140">
            <template #default="{ row }">{{ windowText(row.five_hour) }}</template>
          </el-table-column>

          <el-table-column label="本周用量" width="140">
            <template #default="{ row }">{{ windowText(row.weekly) }}</template>
          </el-table-column>

          <el-table-column label="状态" min-width="210">
            <template #default="{ row }">
              <el-tag
                v-if="isPreferred(provider, row)"
                type="primary"
                size="small"
                effect="dark"
                class="quota-preferred"
              >
                当前优先
              </el-tag>
              <el-tag :type="quotaState(row).type" size="small" effect="plain">
                {{ quotaState(row).label }}
              </el-tag>
              <div v-if="row.error" class="gw-hint quota-reason">{{ row.error }}</div>
            </template>
          </el-table-column>
        </el-table>

        <div v-else class="gw-hint quota-empty">
          {{
            provider.enabled
              ? '还没有读到额度。点击「刷新额度」立即读取一次；后台也会自动读取。'
              : '供应商已停用；启用后后台会自动读取额度。'
          }}
        </div>
      </div>

      <div class="gw-section models-head">
        <span class="gw-section-title">模型</span>
        <span class="gw-section-note">
          别名是客户端请求时使用的 model 名；上游模型名是这家供应商自己的叫法。
        </span>
        <div class="grow" />
        <el-button size="small" :icon="Download" @click="openModelFetch(provider)">获取模型</el-button>
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

    <el-dialog v-model="modelDialog.open" title="从上游获取模型" width="520px">
      <div v-if="modelDialog.loading" class="gw-hint">正在请求上游的模型列表…</div>
      <template v-else>
        <div v-if="modelDialog.models.length" class="model-picks">
          <el-checkbox-group v-model="modelDialog.picked">
            <el-checkbox v-for="id in modelDialog.models" :key="id" :value="id">{{ id }}</el-checkbox>
          </el-checkbox-group>
        </div>
        <div v-else class="gw-empty">上游没有返回模型。</div>
        <p class="gw-hint dialog-lead">别名默认与上游模型名相同，添加后可以再改。</p>
      </template>
      <template #footer>
        <el-button size="small" @click="modelDialog.open = false">取消</el-button>
        <el-button
          size="small"
          type="primary"
          :disabled="!modelDialog.picked.length"
          @click="addPickedModels"
        >
          添加所选（{{ modelDialog.picked.length }}）
        </el-button>
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

.model-picks {
  display: flex;
  flex-direction: column;
  max-height: 320px;
  overflow: auto;
  padding: 4px 2px;
}

.model-picks :deep(.el-checkbox) {
  margin-right: 0;
  height: 28px;
}

.models-head {
  margin-top: 20px;
}

.quota {
  margin-top: 20px;
}

.quota-head {
  margin-bottom: 10px;
}

.quota-account {
  font-weight: 500;
}

.quota-preferred {
  margin-right: 6px;
}

.quota-reason {
  margin-top: 4px;
  word-break: break-all;
}

.quota-empty {
  padding: 8px 0;
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
