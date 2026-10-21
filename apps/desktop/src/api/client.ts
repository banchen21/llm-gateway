/**
 * Talks to the gateway's administrative API.
 *
 * The GUI is served from three different origins depending on how it is run — Vite's dev
 * server (proxied), the gateway itself (same origin), and the desktop shell (whose asset
 * protocol cannot reach the gateway by a relative path). The desktop shell injects
 * `__LLM_GATEWAY_BASE__` to cover the last case; the other two work with a relative path.
 */
import type {
  ConfigEnvelope,
  GatewayConfig,
  Health,
  LogEntry,
  Provider,
  ProviderQuota,
} from './types'

export type {
  ConfigEnvelope,
  GatewayConfig,
  Health,
  LogEntry,
  ProviderQuota,
} from './types'

declare global {
  interface Window {
    __LLM_GATEWAY_BASE__?: string
  }
}

const TOKEN_KEY = 'llm-gateway-token'

function base(): string {
  return window.__LLM_GATEWAY_BASE__ ?? ''
}

/** The gateway origin, for pages that call the inference endpoints directly. */
export function gatewayBase(): string {
  return base()
}

export function getToken(): string {
  return localStorage.getItem(TOKEN_KEY) ?? ''
}

export function setToken(token: string): void {
  if (token) {
    localStorage.setItem(TOKEN_KEY, token)
  } else {
    localStorage.removeItem(TOKEN_KEY)
  }
}

/** An error carrying the gateway's own message, which is what the user needs to see. */
export class GatewayError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message)
  }
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const token = getToken()
  const response = await fetch(`${base()}${path}`, {
    ...init,
    headers: {
      'content-type': 'application/json',
      ...(token ? { 'x-gateway-token': token } : {}),
      ...init.headers,
    },
  })

  const text = await response.text()
  if (!response.ok) {
    throw new GatewayError(extractMessage(text, response.status), response.status)
  }
  return text ? (JSON.parse(text) as T) : (undefined as T)
}

/** The gateway answers errors in the OpenAI envelope, with the reason in `message`. */
function extractMessage(text: string, status: number): string {
  try {
    const value = JSON.parse(text)
    const message = value?.error?.message ?? value?.error
    if (typeof message === 'string') return message
  } catch {
    // The body was not JSON; fall through to the status line.
  }
  return `请求失败 (HTTP ${status})`
}

export function fetchConfig(): Promise<ConfigEnvelope> {
  return request<ConfigEnvelope>('/admin/config')
}

export function saveConfig(config: GatewayConfig): Promise<{ saved: boolean }> {
  return request<{ saved: boolean }>('/admin/config', {
    method: 'PUT',
    body: JSON.stringify(config),
  })
}

export function fetchHealth(): Promise<Health> {
  return request<Health>('/admin/health')
}

export function fetchLogs(limit = 200): Promise<{ entries: LogEntry[] }> {
  return request<{ entries: LogEntry[] }>(`/admin/logs?limit=${limit}`)
}

/** Empties the request log and reports how many records went. */
export function clearLogs(): Promise<{ cleared: number }> {
  return request<{ cleared: number }>('/admin/logs', { method: 'DELETE' })
}

/**
 * Asks the upstream endpoint which models it serves.
 *
 * The provider document is sent as it stands in the editor, so it works before saving.
 */
export function fetchProviderModels(provider: Provider): Promise<{ models: string[] }> {
  return request<{ models: string[] }>('/admin/provider-models', {
    method: 'POST',
    body: JSON.stringify({
      protocol: provider.protocol,
      base_url: provider.base_url,
      api_key: provider.api_key,
      api_keys: provider.api_keys,
      custom_headers: provider.custom_headers,
    }),
  })
}

/** The quota readings the gateway already has, without asking any upstream again. */
export function fetchQuotas(): Promise<{ providers: ProviderQuota[] }> {
  return request<{ providers: ProviderQuota[] }>('/admin/provider-quota')
}

/**
 * Reads a provider's account quotas now, including for an unsaved draft.
 *
 * `supported` is false for endpoints whose quota API the gateway does not know.
 */
export function refreshProviderQuota(
  provider: Provider,
): Promise<{ supported: boolean; quota: ProviderQuota | null }> {
  return request<{ supported: boolean; quota: ProviderQuota | null }>(
    '/admin/provider-quota',
    {
      method: 'POST',
      body: JSON.stringify({
        id: provider.id,
        name: provider.name,
        protocol: provider.protocol,
        base_url: provider.base_url,
        api_key: provider.api_key,
        api_keys: provider.api_keys,
        custom_headers: provider.custom_headers,
      }),
    },
  )
}
