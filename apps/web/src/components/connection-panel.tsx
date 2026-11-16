import { useEffect, useState, type FormEvent } from 'react'
import { parsePairingCode, relayUrl, relayScheme, type PairingCode } from '@waku/client/pairing'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { StartupScreen } from '@/components/startup-screen'
import { WakuIcon } from '@/components/waku-icon'
import { useDocumentTitle } from '@/hooks/use-document-title'
import { useDaemon } from '@/lib/daemon-context'
import { useI18n } from '@/lib/i18n'
import wakuAppIconUrl from '../../../../website/public/app-icon.png'

type ConnectMode = 'address' | 'pairing'

export function ConnectionPanel({ title }: { title?: string } = {}) {  const { t } = useI18n()
  const { config, phase, error, connect } = useDaemon()
  useDocumentTitle(title)
  const [mode, setMode] = useState<ConnectMode>('address')
  const [address, setAddress] = useState(config?.address ?? '')
  const [pairingCode, setPairingCode] = useState('')
  const [token, setToken] = useState(config?.token ?? '')
  const [tokenRevealed, setTokenRevealed] = useState(false)
  const [remember, setRemember] = useState(config?.remember ?? false)
  const [localError, setLocalError] = useState<string | null>(null)

  useEffect(() => {
    if (!config) return
    setAddress(config.address)
    setToken(config.token)
    setTokenRevealed(false)
    setRemember(config.remember)
    // A stored connection that carries a pin was made from a pairing code, so
    // the panel opens on that tab rather than pretending it was an address.
    if (config.fingerprint) setMode('pairing')
  }, [config])

  if (phase === 'booting' || (phase === 'connecting' && config)) {
    return <StartupScreen />
  }

  async function submit(event: FormEvent) {
    event.preventDefault()
    setLocalError(null)
    if (mode === 'pairing') {
      let parsed: PairingCode
      try {
        parsed = parsePairingCode(pairingCode)
      } catch (cause) {
        // A code that does not parse is refused here rather than connected to
        // without a pin, which is the whole attack this prevents.
        setLocalError(
          t('pairing.parse_failed', {
            error: cause instanceof Error ? cause.message : String(cause),
          }),
        )
        return
      }
      try {
        // The scheme comes from the rule, never from the code: a code able to
        // name `ws://` would be one an attacker rewrites to strip transport
        // encryption while the fingerprint check still passed.
        await connect({
          address: relayUrl(parsed),
          token,
          remember,
          fingerprint: parsed.fingerprint,
        })
      } catch (cause) {
        setLocalError(cause instanceof Error ? cause.message : String(cause))
      }
      return
    }
    try {
      await connect({ address, token, remember })
    } catch (cause) {
      setLocalError(cause instanceof Error ? cause.message : String(cause))
    }
  }

  return (
    <main className="flex min-h-dvh w-full items-center justify-center overflow-y-auto bg-background p-5 pb-12">
      <div className="w-full max-w-[520px]">
        <div className="text-center">
          <img
            alt="Waku"
            className="mx-auto size-8 rounded-[8px]"
            draggable={false}
            src={wakuAppIconUrl}
          />
          <h1 className="mt-3 text-xl font-medium tracking-tight">{t('web.connect_title')}</h1>
          <p className="mx-auto mt-2 max-w-sm text-[12.5px] leading-[19px] text-[var(--text-tertiary)]">
            {t('web.connect_description')}
          </p>
        </div>

        <section className="mt-6 rounded-[13px] bg-[var(--raised)] p-5">
          <form className="space-y-4" onSubmit={submit}>
            <div className="flex gap-1 rounded-lg bg-[var(--inset)] p-1" role="tablist">
              {(['address', 'pairing'] as const).map((option) => (
                <button
                  aria-selected={mode === option}
                  className={`flex-1 rounded-md px-3 py-1.5 text-[11.5px] font-medium transition-colors ${
                    mode === option
                      ? 'bg-background text-[var(--text)] shadow-sm'
                      : 'text-[var(--text-tertiary)] hover:text-[var(--text-secondary)]'
                  }`}
                  key={option}
                  role="tab"
                  type="button"
                  onClick={() => {
                    setMode(option)
                    setLocalError(null)
                  }}
                >
                  {option === 'pairing' ? t('pairing.code_label') : t('daemon.websocket_url')}
                </button>
              ))}
            </div>

            {mode === 'address' ? (
              <label className="grid gap-1.5 sm:grid-cols-[120px_1fr] sm:items-center">
                <span className="text-[12px] font-medium">{t('daemon.websocket_url')}</span>
                <Input
                  autoCapitalize="none"
                  autoCorrect="off"
                  className="bg-card"
                  inputMode="url"
                  placeholder="wss://waku.example.com"
                  value={address}
                  onChange={(event) => setAddress(event.target.value)}
                />
              </label>
            ) : (
              <div className="grid gap-1.5">
                <label className="text-[12px] font-medium" htmlFor="daemon-pairing-code">
                  {t('pairing.code_label')}
                </label>
                <Input
                  autoCapitalize="none"
                  autoCorrect="off"
                  className="bg-card font-mono text-[12px]"
                  id="daemon-pairing-code"
                  placeholder={t('pairing.code_placeholder')}
                  spellCheck={false}
                  value={pairingCode}
                  onChange={(event) => setPairingCode(event.target.value)}
                />
                <p className="text-[10.5px] leading-4 text-[var(--text-tertiary)]">
                  {t('pairing.code_hint')}
                </p>
                {pairingCode.trim() ? <PairingSchemeHint code={pairingCode} /> : null}
                <p className="text-[10.5px] leading-4 text-[var(--text-tertiary)]">
                  {t('pairing.one_time_notice')}
                </p>
              </div>
            )}
            <div className="grid gap-1.5 sm:grid-cols-[120px_1fr] sm:items-center">
              <label className="text-[12px] font-medium" htmlFor="daemon-connection-token">
                {t('daemon.token')}
              </label>
              <div className="relative">
                <Input
                  autoComplete="current-password"
                  className="bg-card pr-10"
                  id="daemon-connection-token"
                  placeholder={t('web.token_placeholder')}
                  type={tokenRevealed ? 'text' : 'password'}
                  value={token}
                  onChange={(event) => setToken(event.target.value)}
                />
                <Button
                  aria-label={t(tokenRevealed ? 'daemon.hide_token' : 'daemon.reveal_token')}
                  aria-pressed={tokenRevealed}
                  className="absolute right-1 top-1 size-7 text-[var(--text-tertiary)]"
                  size="icon-sm"
                  title={t(tokenRevealed ? 'daemon.hide_token' : 'daemon.reveal_token')}
                  type="button"
                  variant="ghost"
                  onClick={() => setTokenRevealed((revealed) => !revealed)}
                >
                  <WakuIcon name={tokenRevealed ? 'eyeOff' : 'eye'} />
                </Button>
              </div>
            </div>
            <label className="flex cursor-pointer items-start gap-2.5 border-t pt-4 text-[12px]">
              <input
                checked={remember}
                className="mt-0.5 size-4 accent-foreground"
                type="checkbox"
                onChange={(event) => setRemember(event.target.checked)}
              />
              <span>
                {t('web.remember_device')}
                <span className="mt-0.5 block text-[10.5px] leading-4 text-[var(--text-tertiary)]">
                  {t('web.remember_device_description')}
                </span>
              </span>
            </label>
            {(localError || error) && (
              <div role="alert" className="rounded-lg bg-[var(--danger-soft)] px-3 py-2 text-[12px] text-destructive">
                {localError || error}
              </div>
            )}
            <div className="flex justify-end">
              <Button className="rounded-full px-4" type="submit" disabled={phase === 'connecting'}>
                {phase === 'connecting' ? t('web.connecting') : t('web.connect')}
                {phase !== 'connecting' && <WakuIcon name="arrowRight" />}
              </Button>
            </div>
          </form>
        </section>

        <div className="mt-3 flex gap-2 rounded-lg bg-accent px-3 py-2 text-[10.5px] leading-4 text-[var(--text-tertiary)]">
          <WakuIcon className="mt-0.5 size-3.5 shrink-0" name="lock" />
          <p>
            {t('web.security_warning')}
          </p>
        </div>
      </div>
    </main>
  )
}

/**
 * Tells the user whether the relay will be dialed over TLS, before they submit.
 *
 * The scheme is derived from the code's relay host, never read from the code,
 * so this is the one place a user can see which of the two rules applied —
 * useful when a self-hosted relay on the same machine legitimately uses `ws`.
 */
function PairingSchemeHint({ code }: { code: string }) {
  const { t } = useI18n()
  let text: string | null = null
  try {
    const parsed = parsePairingCode(code)
    text = t(relayScheme(parsed) === 'ws' ? 'pairing.relay_scheme_ws' : 'pairing.relay_scheme_wss')
  } catch {
    // A code that does not parse gets its error from submit; a scheme hint
    // alongside it would be noise.
    text = null
  }
  if (!text) return null
  return (
    <p className="text-[10.5px] leading-4 text-[var(--text-tertiary)]">
      {text} · <span className="font-mono">{safeRelayUrl(code)}</span>
    </p>
  )
}

function safeRelayUrl(code: string): string {
  try {
    return relayUrl(parsePairingCode(code))
  } catch {
    return ''
  }
}
