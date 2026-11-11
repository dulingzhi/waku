import { useEffect } from 'react'

/**
 * The browser tab's product name. Like the window title on desktop, it is a
 * proper noun and stays untranslated, so it is also the separator's anchor.
 */
export const WAKU_DOCUMENT_TITLE = 'Waku Web'

export function formatDocumentTitle(section?: string | null): string {
  const normalized = section?.trim()
  if (!normalized || normalized === WAKU_DOCUMENT_TITLE) return WAKU_DOCUMENT_TITLE
  return `${normalized} — ${WAKU_DOCUMENT_TITLE}`
}

/**
 * The section is already-localized copy supplied by the caller; only the
 * product name and the separator are fixed here.
 */
export function useDocumentTitle(section?: string | null) {
  const title = formatDocumentTitle(section)
  useEffect(() => {
    document.title = title
  }, [title])
}
