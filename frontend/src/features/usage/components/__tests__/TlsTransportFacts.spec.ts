import { afterEach, describe, expect, it } from 'vitest'
import { createApp, h, type App } from 'vue'

import TlsTransportFacts from '../TlsTransportFacts.vue'
import { resolveOutgoingTlsFingerprint } from '../../utils/tlsTransport'

const mountedApps: Array<{ app: App, root: HTMLElement }> = []

afterEach(() => {
  for (const { app, root } of mountedApps.splice(0)) {
    app.unmount()
    root.remove()
  }
})

function mount(outgoing: NonNullable<ReturnType<typeof resolveOutgoingTlsFingerprint>>) {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp({ render: () => h(TlsTransportFacts, { outgoing }) })
  app.mount(root)
  mountedApps.push({ app, root })
  return root
}

describe('resolveOutgoingTlsFingerprint', () => {
  it('only accepts gateway-generated outgoing records and drops incoming', () => {
    expect(resolveOutgoingTlsFingerprint(null)).toBeNull()
    expect(resolveOutgoingTlsFingerprint({ tls_fingerprint: { incoming: { ja3: 'x' } } })).toBeNull()
    expect(resolveOutgoingTlsFingerprint({
      tls_fingerprint: { outgoing: { source: 'client', backend: 'reqwest_rustls', observed: true } },
    })).toBeNull()
    const outgoing = resolveOutgoingTlsFingerprint({
      tls_fingerprint: {
        incoming: { ja3: 'must-not-matter' },
        outgoing: {
          source: 'aether_transport_config',
          backend: 'browser_wreq',
          http_mode: 'http1_only',
          tls_stack: 'boringssl_wreq',
          emulation_profile: 'claude_code_node_openssl',
          alpn_offered: ['http/1.1'],
          observed: true,
          ja3_hash: '3f2a1c9e8b7d6f5a4c3b2a1908f7e6d5',
          ja4: 't13d1716h1_5b57614c22b0_3d5db4fb5c1e',
          probed_at_unix_secs: '1760000000',
        },
      },
    })
    expect(outgoing).toMatchObject({
      observed: true,
      backend: 'browser_wreq',
      tls_stack: 'boringssl_wreq',
      emulation_profile: 'claude_code_node_openssl',
      alpn_offered: ['http/1.1'],
      ja4: 't13d1716h1_5b57614c22b0_3d5db4fb5c1e',
      probed_at_unix_secs: 1_760_000_000,
    })
  })
})

describe('TlsTransportFacts', () => {
  it('renders the emulation profile and fingerprints when observed', () => {
    const root = mount({
      observed: true,
      backend: 'browser_wreq',
      http_mode: 'http1_only',
      tls_stack: 'boringssl_wreq',
      emulation_profile: 'claude_code_node_openssl',
      alpn_offered: ['http/1.1'],
      ja3_hash: '3f2a1c9e8b7d6f5a4c3b2a1908f7e6d5',
      ja4: 't13d1716h1_5b57614c22b0_3d5db4fb5c1e',
      probed_at_unix_secs: 1_760_000_000,
      probe_url: 'https://tls.peet.ws/api/all',
    })
    expect(root.querySelector('[data-testid="tls-transport-facts"]')).not.toBeNull()
    expect(root.querySelector('[data-testid="tls-transport-stack"]')?.textContent?.trim()).toBe('BoringSSL (wreq)')
    expect(root.querySelector('[data-testid="tls-transport-emulation"]')?.textContent?.trim()).toBe('Claude Code Node/OpenSSL')
    expect(root.querySelector('[data-testid="tls-transport-ja4"]')?.textContent?.trim()).toBe('t13d1716h1_5b57614c22b0_3d5db4fb5c1e')
    expect(root.querySelector('[data-testid="tls-transport-ja3-hash"]')?.textContent).toContain('3f2a1c9e8b7d6f5a4c3b2a1908f7e6d5')
    expect(root.querySelector('[data-testid="tls-transport-unobserved"]')).toBeNull()
    expect([...root.querySelectorAll('dd')][2].textContent).toContain('ALPN http/1.1')
  })

  it('renders the config-inferred state when no probe has been run', () => {
    const root = mount({
      observed: false,
      backend: 'reqwest_rustls',
      http_mode: 'auto',
      tls_stack: 'rustls',
      emulation_profile: null,
      alpn_offered: ['h2', 'http/1.1'],
    })
    expect(root.querySelector('[data-testid="tls-transport-stack"]')?.textContent?.trim()).toBe('rustls')
    expect(root.querySelector('[data-testid="tls-transport-emulation"]')?.textContent?.trim()).toBe('无（系统默认）')
    expect(root.querySelector('[data-testid="tls-transport-unobserved"]')?.textContent).toContain('未探测')
    expect(root.querySelector('[data-testid="tls-transport-unobserved"]')?.textContent).toContain('reqwest_rustls')
    expect(root.querySelector('[data-testid="tls-transport-ja4"]')).toBeNull()
  })
})
