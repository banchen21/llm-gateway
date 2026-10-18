/**
 * Talks to the gateway's administrative API.
 *
 * The GUI is served from three different origins depending on how it is run — Vite's dev
 * server (proxied), the gateway itself (same origin), and the desktop shell (whose asset
 * protocol cannot reach the gateway by a relative path). The desktop shell injects
 * `__LLM_GATEWAY_BASE__` to cover the last case; the other two work with a relative path.
 */
import type { ConfigEnvelope, GatewayConfig, Health, LogEntry } from './types'

export type { ConfigEnvelope, GatewayConfig, Health, LogEntry } from './types'

declare global {
  interface Window {
    __LLM_GATEWAY_BASE__?: string
  }
}

const TOKEN_KEY = 'llm-gateway-token'

function base(): string {
  return window.__LLM_GATEWAY_BASE__ ?? ''
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
