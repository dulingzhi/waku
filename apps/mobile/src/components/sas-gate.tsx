import { useCallback, useEffect, useState } from 'react';
import { Modal, Pressable, StyleSheet, Text, View } from 'react-native';

import { GlassSurface } from '@/components/glass-surface';
import { Radius, Spacing } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useDaemon } from '@/lib/daemon-context';
import { useI18n } from '@/lib/i18n-context';

/**
 * The short authentication string, shown once per connection and blocking the
 * app until the user says the two screens agree.
 *
 * **There is no dismiss or skip**, and the backdrop does not close it. The
 * string is the only check that catches a relay running one handshake per
 * side — such a relay produces two different transcripts, so the two screens
 * show two different strings. A dialog a user can wave through without looking
 * leaves that check unperformed while looking implemented, which is the exact
 * failure this screen exists to prevent.
 *
 * The two buttons are "they match" and "they do not", and the second one
 * disconnects.
 */
export function SasGate() {
  const theme = useTheme();
  const { t } = useI18n();
  const { authenticationString, disconnect, phase } = useDaemon();
  const [confirmed, setConfirmed] = useState(false);

  // A new connection is a new handshake, so a confirmation from the previous
  // one says nothing about this one.
  useEffect(() => {
    if (phase !== 'connected') setConfirmed(false);
  }, [phase]);

  const confirm = useCallback(() => setConfirmed(true), []);
  const mismatch = useCallback(() => {
    setConfirmed(false);
    disconnect();
  }, [disconnect]);

  const visible = Boolean(authenticationString) && !confirmed;
  if (!visible || !authenticationString) return null;

  return (
    <Modal
      animationType="fade"
      onRequestClose={() => {
        // Android's back button must not dismiss this: the comparison is not
        // optional, so the only ways out are the two buttons.
      }}
      transparent
      visible>
      <View style={styles.backdrop}>
        <GlassSurface fallbackColor={theme.surface} style={styles.card}>
          <View style={styles.inner}>
            <Text style={[styles.title, { color: theme.text }]}>
              {t('pairing.sas_title')}
            </Text>
            <Text style={[styles.body, { color: theme.textSecondary }]}>
              {t('pairing.sas_body')}
            </Text>
            <View style={[styles.digits, { backgroundColor: theme.inset }]}>
              <Text style={[styles.digitsText, { color: theme.text }]}>
                {authenticationString}
              </Text>
            </View>
            <Pressable
              accessibilityRole="button"
              onPress={mismatch}
              style={({ pressed }) => [styles.secondary, { opacity: pressed ? 0.6 : 1 }]}>
              <Text style={[styles.secondaryText, { color: theme.danger }]}>
                {t('pairing.sas_mismatch')}
              </Text>
            </Pressable>
            <Pressable
              accessibilityRole="button"
              onPress={confirm}
              style={({ pressed }) => [
                styles.primary,
                { backgroundColor: theme.inverse, opacity: pressed ? 0.7 : 1 },
              ]}>
              <Text style={[styles.primaryText, { color: theme.onInverse }]}>
                {t('pairing.sas_confirmed')}
              </Text>
            </Pressable>
          </View>
        </GlassSurface>
      </View>
    </Modal>
  );
}

const styles = StyleSheet.create({
  backdrop: {
    alignItems: 'center',
    backgroundColor: 'rgba(0,0,0,0.45)',
    flex: 1,
    justifyContent: 'center',
    padding: Spacing.four,
  },
  card: { borderRadius: Radius.large, maxWidth: 420, width: '100%' },
  inner: { gap: 12, padding: Spacing.four },
  title: { fontSize: 17, fontWeight: '600' },
  body: { fontSize: 13, lineHeight: 19 },
  digits: {
    alignItems: 'center',
    borderRadius: Radius.medium,
    marginVertical: 6,
    paddingVertical: 16,
  },
  digitsText: {
    fontSize: 24,
    fontVariant: ['tabular-nums'],
    fontWeight: '600',
    letterSpacing: 3,
  },
  primary: {
    alignItems: 'center',
    borderRadius: Radius.pill,
    justifyContent: 'center',
    minHeight: 46,
  },
  primaryText: { fontSize: 15, fontWeight: '600' },
  secondary: { alignItems: 'center', justifyContent: 'center', minHeight: 40 },
  secondaryText: { fontSize: 14, fontWeight: '500' },
});
