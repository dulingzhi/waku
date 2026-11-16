import { Button } from '@/components/ui/button'
import { WakuIcon } from '@/components/waku-icon'
import { useI18n } from '@/lib/i18n'

/**
 * The short authentication string, shown once per connection and blocking the
 * app until the user says the two screens agree.
 *
 * **There is no dismiss or skip.** The string is the only check that catches a
 * relay running one handshake per side — such a relay produces two different
 * transcripts, so the two screens show two different strings. A dialog a user
 * can wave through without looking, or dismiss, leaves that check unperformed
 * while looking implemented. The two buttons are therefore "they match" and
 * "they do not", and the second one disconnects.
 *
 * Shown once rather than permanently because the comparison happens once, at
 * the moment the connection is made; a permanent banner would be read the
 * first time and ignored afterwards.
 */
export function SasDialog({
  digits,
  onConfirm,
  onMismatch,
}: {
  digits: string
  onConfirm: () => void
  onMismatch: () => void
}) {
  const { t } = useI18n()
  return (
    <div
      aria-labelledby="sas-title"
      aria-modal="true"
      className="fixed inset-0 z-[200] grid place-items-center bg-black/45 p-5"
      role="dialog"
    >
      <div className="w-full max-w-[420px] rounded-[14px] bg-background p-6 shadow-xl">
        <div className="flex items-start gap-3">
          <div className="grid size-9 shrink-0 place-items-center rounded-[9px] bg-[var(--inset)]">
            <WakuIcon className="size-4 text-[var(--text-secondary)]" name="lock" />
          </div>
          <div className="min-w-0">
            <h2 className="text-[14px] font-medium" id="sas-title">
              {t('pairing.sas_title')}
            </h2>
            <p className="mt-1.5 text-[11.5px] leading-[17px] text-[var(--text-secondary)]">
              {t('pairing.sas_body')}
            </p>
          </div>
        </div>

        <div
          aria-label={digits}
          className="my-5 rounded-[10px] bg-[var(--inset)] py-4 text-center font-mono text-[22px] tracking-[0.18em] tabular-nums"
        >
          {digits}
        </div>

        <div className="flex flex-col gap-2 sm:flex-row sm:justify-end">
          <Button variant="ghost" onClick={onMismatch}>
            {t('pairing.sas_mismatch')}
          </Button>
          <Button className="rounded-full px-4" onClick={onConfirm}>
            {t('pairing.sas_confirmed')}
          </Button>
        </div>
      </div>
    </div>
  )
}
