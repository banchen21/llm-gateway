<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { CopyDocument } from '@element-plus/icons-vue'
import { useGateway } from '../store'
import { fetchHealth, type Health } from '../api/client'
import {
  endpointsFor,
  PROTOCOL_MODE_LABELS,
  PROTOCOL_MODE_SUMMARY,
  type ProtocolMode,
} from '../api/types'

const gateway = useGateway()
const { config } = gateway
const health = ref<Health | null>(null)

const modes = Object.keys(PROTOCOL_MODE_LABELS) as ProtocolMode[]

/** The endpoints in effect right now, which may predate unsaved edits. */
const liveEndpoints = computed(() => endpointsFor(health.value?.protocol_mode ?? 'primary_anthropic'))
const draftEndpoints = computed(() => endpointsFor(config.protocol_mode))
const pendingModeChange = computed(
  () => health.value !== null && health.value.protocol_mode !== config.protocol_mode,
)

const baseUrl = computed(() => `http://${config.server.bind}:${config.server.port}`)
const needsToken = computed(
  () => config.web_ui.enabled && !isLoopback(config.server.bind) && !config.web_ui.token,
)

onMounted(refresh)

async function refresh() {
  try {
    health.value = await fetchHealth()
  } catch {
    health.value = null
  }
}

function toggleSlow(enabled: boolean) {
  config.slow_after_ms = enabled ? 5000 : 0
}

function isLoopback(bind: string): boolean {
  const value = bind.trim()
  return value === 'localhost' || value === '127.0.0.1' || value === '::1'
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
  <div class="grid">
    <el-card shadow="never">
      <template #header>
        <span class="card-title">协议模式</span>
      </template>
      <p class="gw-hint">
        模式决定网关<strong>对外开放哪些入站接口</strong>。上游供应商是 Anthropic 型还是 OpenAI
        型不受它限制：只要在供应商页配置了，任何模式下都能被路由到，协议差异由网关转换。
      </p>

      <el-radio-group v-model="config.protocol_mode" class="modes">
        <el-radio v-for="mode in modes" :key="mode" :value="mode" class="mode">
          <div class="mode-body">
            <span class="mode-name">{{ PROTOCOL_MODE_LABELS[mode] }}</span>
            <span class="gw-hint">{{ PROTOCOL_MODE_SUMMARY[mode] }}</span>
            <span class="mono mode-endpoints">{{ endpointsFor(mode).join('  ') }}</span>
          </div>
        </el-radio>
      </el-radio-group>

      <el-alert
        v-if="pendingModeChange"
        type="info"
        show-icon
        :closable="false"
        title="模式已改动但尚未保存"
        :description="`网关当前仍开放：${liveEndpoints.join('、')}；保存后将切换为：${draftEndpoints.join('、')}。切换无需重启。`"
      />
      <div v-else class="gw-hint live">
        当前生效：<span class="mono">{{ liveEndpoints.join('  ') }}</span>
      </div>
    </el-card>

    <el-card shadow="never">
      <template #header>
        <span class="card-title">监听地址</span>
      </template>
      <el-form label-width="128px" label-position="left" size="small">
        <el-form-item label="绑定地址">
          <el-input v-model="config.server.bind" placeholder="127.0.0.1" />
          <div class="gw-hint">
            保持 127.0.0.1 则只有本机能访问。改成本机以外的地址前请先设置管理令牌。
          </div>
        </el-form-item>
        <el-form-item label="端口">
          <el-input-number v-model="config.server.port" :min="1" :max="65535" :controls="false" />
        </el-form-item>
        <el-form-item label="入站 API Key">
          <el-input
            v-model="config.server.api_key"
            type="password"
            show-password
            placeholder="留空则不校验"
            clearable
          />
          <div class="gw-hint">
            留空时任何调用方都能使用网关。填上之后，调用方需要带上 x-api-key 或 Authorization:
            Bearer。Anthropic SDK 要求必须传一个 key，所以这里填一个占位值也很常见。
          </div>
        </el-form-item>
        <el-alert
          v-if="health?.proxy_from_env"
          type="warning"
          show-icon
          :closable="false"
          title="上游请求会经过代理"
          description="当前进程设置了 ALL_PROXY / HTTP(S)_PROXY。代理退出后上游会报 tunnel error；清除环境变量并重启网关即可恢复直连。"
        />
        <el-alert
          v-else-if="health?.system_proxy_available"
          type="info"
          show-icon
          :closable="false"
          title="直连失败时会自动尝试系统代理"
          :description="`检测到 Windows 已开启系统代理。当前优先：${health?.preferred_path === 'proxy' ? '代理' : '直连'}（按最近实测表现决定）；两者都不通时会在 Windows 通知中心提示网络问题。`"
        />
      </el-form>
    </el-card>

    <el-card shadow="never" class="wide">
      <template #header>
        <span class="card-title">Web 界面</span>
      </template>
      <el-form label-width="128px" label-position="left" size="small">
        <el-form-item label="启用">
          <el-switch v-model="config.web_ui.enabled" />
          <span class="gw-hint inline">
            开启后可以像现在这样在浏览器里使用这个界面，地址就是上面的监听地址。
          </span>
        </el-form-item>
        <el-form-item label="管理令牌">
          <el-input
            v-model="config.web_ui.token"
            type="password"
            show-password
            placeholder="留空则不校验（仅回环地址可用）"
            clearable
          />
          <div class="gw-hint">
            管理接口可以读写供应商密钥，所以只要绑定地址不是回环地址，就必须设置令牌，否则保存时会被拒绝。
          </div>
        </el-form-item>
      </el-form>

      <el-alert
        v-if="needsToken"
        type="warning"
        show-icon
        :closable="false"
        title="保存会被拒绝"
        :description="`绑定地址 ${config.server.bind} 不是回环地址，启用 Web 界面时必须设置管理令牌。`"
      />
      <div v-else-if="config.web_ui.enabled" class="access">
        <span class="gw-hint">浏览器访问</span>
        <span class="mono">{{ baseUrl }}</span>
        <el-button text size="small" :icon="CopyDocument" @click="copy(baseUrl)">复制</el-button>
      </div>
      <div v-else class="gw-hint live">
        当前关闭。这个桌面窗口仍可正常使用，只是不能通过浏览器访问同一个界面。
      </div>
    </el-card>

    <el-card shadow="never" class="wide">
      <template #header>
        <span class="card-title">默认供应商</span>
      </template>
      <p class="gw-hint">
        客户端请求的模型名如果没有匹配到任何别名，就交给这里的供应商处理。留空时，未知模型直接返回 404。
      </p>
      <el-select
        v-model="config.default_provider_id"
        placeholder="不设置兜底"
        clearable
        size="small"
        class="default-provider"
      >
        <el-option
          v-for="option in gateway.providerOptions.value"
          :key="option.value"
          :label="option.label"
          :value="option.value"
          :disabled="!option.value"
        />
      </el-select>
      <div v-if="!config.providers.length" class="gw-hint">还没有供应商可选，请先在供应商页添加。</div>

      <el-divider />

      <div class="failover">
        <el-switch v-model="config.failover" />
        <span class="failover-label">额度用尽时自动切换供应商</span>
      </div>
      <p class="gw-hint">
        当别名指向的供应商返回 429（额度）、401（密钥）或 403（套餐）时，按「供应商」页里的顺序尝试其他已启用的供应商，
        并用它们的第一个模型顶替。请求日志会记录实际应答的供应商与模型。
      </p>

      <el-divider />

      <div class="failover">
        <el-switch :model-value="config.slow_after_ms > 0" @update:model-value="toggleSlow" />
        <span class="failover-label">慢请求自动切换</span>
        <el-input-number
          v-if="config.slow_after_ms > 0"
          v-model="config.slow_after_ms"
          :min="1000"
          :max="120000"
          :step="1000"
          size="small"
        />
        <span v-if="config.slow_after_ms > 0" class="gw-hint">毫秒内没有首字节就换一个供应商</span>
      </div>
      <p class="gw-hint">
        「能连上但很慢」不会报错，只有时钟能发现。超过这个时间仍没有首字节时，网关放弃这次调用、
        把该路由标记为缓慢两分钟，并改用其他供应商；期间新请求会直接绕开它。设为 0 关闭。
      </p>
    </el-card>
  </div>
</template>

<style scoped>
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(420px, 1fr));
  gap: 16px;
  align-items: start;
}

.failover {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 8px;
}

.failover-label {
  font-size: 13px;
  font-weight: 500;
}

.wide {
  grid-column: 1 / -1;
}

.card-title {
  font-size: 13px;
  font-weight: 600;
}

.modes {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin-bottom: 16px;
}

.mode {
  height: auto;
  align-items: flex-start;
  padding: 8px 0;
}

.mode-body {
  display: flex;
  flex-direction: column;
  gap: 2px;
  white-space: normal;
}

.mode-name {
  font-size: 13px;
  font-weight: 600;
}

.mode-endpoints {
  color: var(--gw-accent);
}

.live {
  padding-top: 4px;
}

.inline {
  margin-left: 10px;
}

.access {
  display: flex;
  align-items: center;
  gap: 8px;
}

.default-provider {
  width: 320px;
}
</style>
