/**
 * Mirrors the Rust configuration types in `crates/gateway-core/src/config` and
 * `crates/gateway-core/src/upstream`. The snake_case names are the wire format, so they
 * are kept verbatim rather than converted to camelCase.
 */

/** Which inbound protocols the gateway serves. */
export type ProtocolMode = 'primary_anthropic' | 'primary_open_ai' | 'route'

/** Which protocol an upstream provider speaks. */
export type ProviderProtocol = 'anthropic' | 'open_ai'

/** Which Chat Completions field carries the output limit. */
export type MaxTokensField = 'max_tokens' | 'max_completion_tokens'

export interface ProviderModel {
  id: string
  /** The name clients call this model by; unique across enabled providers. */
  alias: string
  /** The provider's own model id, which may differ from the alias. */
  upstream_model: string
  enabled: boolean
  /** Caps output when a client omits `max_tokens`. */
  max_output_tokens: number | null
  max_tokens_field: MaxTokensField
  /** Extra fields merged into the upstream request body. */
  extra: Record<string, unknown>
}

export interface Provider {
  id: string
  name: string
  protocol: ProviderProtocol
  base_url: string
  /** The primary key. */
  api_key: string
  /** Extra keys for the same endpoint, tried when the primary is rate limited. */
  api_keys: ApiKey[]
  enabled: boolean
  custom_headers: Record<string, string>
  models: ProviderModel[]
}

/** One named inbound key, so a client can be revoked without disturbing the others. */
export interface ApiKey {
  /** Assigned by the gateway when the entry has none. */
  id: string
  /** A label for the client the key was issued to. */
  name: string
  key: string
}

export interface ServerConfig {
  bind: string
  port: number
  /** The legacy single key. Kept for configurations written before named keys. */
  api_key: string | null
  /** Named inbound keys; any one of them authenticates a caller. */
  api_keys: ApiKey[]
}

export interface WebUiConfig {
  /** Serves this GUI over the gateway's own port. Off by default. */
  enabled: boolean
  /** Required for administrative access when enabled. */
  token: string | null
}

export interface GatewayConfig {
  protocol_mode: ProtocolMode
  server: ServerConfig
  web_ui: WebUiConfig
  providers: Provider[]
  /** Catch-all for model names no alias matches. */
  default_provider_id: string | null
  /** When a provider refuses (quota, credential, plan), try the other enabled ones. */
  failover: boolean
}

export interface ConfigEnvelope {
  config: GatewayConfig
  /** Every alias the gateway can currently route. */
  aliases: string[]
  /** Where the configuration is stored on disk. */
  path: string
}

export interface Health {
  ok: boolean
  protocol_mode: ProtocolMode
  providers: number
  aliases: number
  web_ui_enabled: boolean
  /** Whether ALL_PROXY / HTTP(S)_PROXY are set for the gateway process. */
  proxy_from_env: boolean
  config_readable: boolean
  inbound_api_key_required: boolean
}

export interface LogEntry {
  at: number
  model: string
  provider: string
  upstream_model: string
  stream: boolean
  status: number
  prompt: {
    system_chars: number
    messages: number
    tool_results: number
    tools: number
  }
  usage?: {
    input_tokens: number
    output_tokens: number
    cache_read_tokens: number
    cache_write_tokens: number
  }
  error?: string
}

/** A provider with no identity yet; the gateway assigns ids on save. */
export function emptyProvider(): Provider {
  return {
    id: '',
    name: '',
    protocol: 'anthropic',
    base_url: '',
    api_key: '',
    api_keys: [],
    enabled: true,
    custom_headers: {},
    models: [],
  }
}

export function emptyModel(): ProviderModel {
  return {
    id: '',
    alias: '',
    upstream_model: '',
    enabled: true,
    max_output_tokens: null,
    // Third-party OpenAI-compatible endpoints overwhelmingly expect `max_tokens`, so
    // that is the default; OpenAI's own reasoning models need the other one.
    max_tokens_field: 'max_tokens',
    extra: {},
  }
}

/** A key with no identity yet; the gateway assigns an id on save. */
export function emptyApiKey(): ApiKey {
  return { id: '', name: '', key: '' }
}

export function emptyConfig(): GatewayConfig {
  return {
    protocol_mode: 'primary_anthropic',
    server: { bind: '127.0.0.1', port: 8787, api_key: null, api_keys: [] },
    web_ui: { enabled: false, token: null },
    providers: [],
    default_provider_id: null,
    failover: false,
  }
}

export const PROTOCOL_MODE_LABELS: Record<ProtocolMode, string> = {
  primary_anthropic: '主协议 · Anthropic',
  primary_open_ai: '主协议 · OpenAI',
  route: '路由协议',
}

export const PROTOCOL_MODE_SUMMARY: Record<ProtocolMode, string> = {
  primary_anthropic: '仅开放 /v1/messages',
  primary_open_ai: '仅开放 /v1/chat/completions',
  route: '两个入站接口同时开放',
}

export function endpointsFor(mode: ProtocolMode): string[] {
  switch (mode) {
    case 'primary_anthropic':
      return ['/v1/messages', '/v1/messages/count_tokens']
    case 'primary_open_ai':
      return ['/v1/chat/completions']
    case 'route':
      return ['/v1/messages', '/v1/messages/count_tokens', '/v1/chat/completions']
  }
}
