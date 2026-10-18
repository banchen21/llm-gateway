/**
 * The single source of truth for the configuration being edited.
 *
 * The gateway reads and writes its configuration as one document, so the GUI does the
 * same: views edit this object in place, and one save publishes all of it.
 */
import { computed, reactive, ref, watch } from 'vue'
import * as api from './api/client'
import { emptyConfig, type GatewayConfig } from './api/types'

const config = reactive<GatewayConfig>(emptyConfig())
const aliases = ref<string[]>([])
const configPath = ref('')
const loading = ref(false)
const saving = ref(false)
const loadError = ref('')
const dirty = ref(false)

/// The last state the gateway acknowledged, so `dirty` reports real unsaved edits.
let savedSnapshot = JSON.stringify(config)

function snapshot(): string {
  return JSON.stringify(config)
}

/** Replaces the editable document in place, keeping the reactive object identity. */
function adopt(next: GatewayConfig): void {
  config.protocol_mode = next.protocol_mode
  config.server = next.server
  config.web_ui = next.web_ui
  config.providers = next.providers
  config.default_provider_id = next.default_provider_id
}

watch(
  config,
  () => {
    dirty.value = snapshot() !== savedSnapshot
  },
  { deep: true },
)

export function useGateway() {
  async function load(): Promise<void> {
    loading.value = true
    loadError.value = ''
    try {
      const envelope = await api.fetchConfig()
      adopt(envelope.config)
      aliases.value = envelope.aliases
      configPath.value = envelope.path
      savedSnapshot = snapshot()
      dirty.value = false
    } catch (error) {
      loadError.value = error instanceof Error ? error.message : String(error)
    } finally {
      loading.value = false
    }
  }

  /** Returns the gateway's complaint when the configuration is rejected, else ''. */
  async function save(): Promise<string> {
    saving.value = true
    try {
      await api.saveConfig(JSON.parse(snapshot()) as GatewayConfig)
      // Ids the gateway assigned and normalizations it applied are part of the truth, so
      // the saved document is re-read rather than assumed.
      await load()
      return ''
    } catch (error) {
      return error instanceof Error ? error.message : String(error)
    } finally {
      saving.value = false
    }
  }

  /** Discards local edits by re-reading what the gateway has. */
  async function revert(): Promise<void> {
    await load()
  }

  return {
    config,
    aliases,
    configPath,
    loading,
    saving,
    loadError,
    dirty: computed(() => dirty.value),
    providerOptions: computed(() =>
      config.providers.map((provider) => ({
        value: provider.id,
        label: provider.name || '(未命名供应商)',
      })),
    ),
    load,
    save,
    revert,
  }
}
