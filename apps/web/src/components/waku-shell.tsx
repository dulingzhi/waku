import { lazy, Suspense, useEffect, useState } from 'react'
import { ConnectionPanel } from '@/components/connection-panel'
import { SasDialog } from '@/components/sas-dialog'
import { StartupScreen } from '@/components/startup-screen'
import { useDaemon } from '@/lib/daemon-context'

const loadConnectedApp = () => import('@/components/waku-app')
const ConnectedWakuApp = lazy(() => loadConnectedApp().then((module) => ({
  default: module.WakuApp,
})))

export function WakuShell() {
  const { config, phase, authenticationString, disconnect } = useDaemon()
  const [sasConfirmed, setSasConfirmed] = useState(false)

  useEffect(() => {
    if (phase === 'connecting' && config) void loadConnectedApp()
  }, [config, phase])

  // A new connection is a new handshake, so a confirmation from the previous
  // one says nothing about this one. Reset on every transition out of
  // `connected` rather than on the SAS value, because a reconnect can
  // legitimately produce the same digits only if the transcript repeated.
  useEffect(() => {
    if (phase !== 'connected') setSasConfirmed(false)
  }, [phase])

  if (phase !== 'connected') return <ConnectionPanel />
  return (
    <>
      <Suspense fallback={<StartupScreen />}>
        <ConnectedWakuApp />
      </Suspense>
      {authenticationString && !sasConfirmed ? (
        <SasDialog
          digits={authenticationString}
          onConfirm={() => setSasConfirmed(true)}
          onMismatch={disconnect}
        />
      ) : null}
    </>
  )
}
