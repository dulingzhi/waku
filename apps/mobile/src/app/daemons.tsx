import * as Haptics from 'expo-haptics';
import { router, Stack } from 'expo-router';
import { navigateBack } from '@/components/screen-header';
import { useState } from 'react';
import {
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  View,
} from 'react-native';

import { AppSymbol } from '@/components/app-symbol';
import { ConnectionBanner } from '@/components/connection-banner';
import { DaemonList } from '@/components/daemon-list';
import { LanguagePickerRow } from '@/components/language-picker';
import { NativeTint } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useDaemon } from '@/lib/daemon-context';
import type { DaemonProfile } from '@/lib/daemon-profile';
import { useI18n } from '@/lib/i18n-context';

export default function DaemonsScreen() {
  const theme = useTheme();
  const daemon = useDaemon();
  const { t } = useI18n();
  const [selectingId, setSelectingId] = useState<string | null>(null);

  async function select(profile: DaemonProfile) {
    if (selectingId) return;
    if (profile.id === daemon.activeProfile?.id) {
      navigateBack();
      return;
    }
    setSelectingId(profile.id);
    try {
      await Haptics.selectionAsync();
      const connected = await daemon.selectProfile(profile.id);
      if (connected) navigateBack();
      else await Haptics.notificationAsync(Haptics.NotificationFeedbackType.Error);
    } finally {
      setSelectingId(null);
    }
  }

  return (
    <View style={[styles.screen, { backgroundColor: theme.background }]}>
      <Stack.Screen
        options={{
          headerRight: () => (
            <Pressable
              accessibilityLabel={t('daemon.add_button')}
              accessibilityRole="button"
              hitSlop={10}
              onPress={() => router.push('/daemon-editor')}>
              <AppSymbol
                name={{ ios: 'plus', android: 'add', web: 'add' }}
                size={21}
                tintColor={NativeTint}
              />
            </Pressable>
          ),
          unstable_headerRightItems: () => [{
            type: 'button',
            accessibilityLabel: t('daemon.add_button'),
            icon: { type: 'sfSymbol', name: 'plus' },
            label: t('daemon.add_button'),
            onPress: () => router.push('/daemon-editor'),
          }],
        }}
      />
      <ScrollView
        contentInsetAdjustmentBehavior="automatic"
        contentContainerStyle={styles.listContent}
        showsVerticalScrollIndicator={false}>
        <ConnectionBanner />
        {daemon.profiles.length ? (
          <>
            <Text style={[styles.sectionTitle, { color: theme.textTertiary }]}>
              {t('daemon.list_title')}
            </Text>
            <DaemonList
              selectingId={selectingId}
              onEdit={(profile) => {
                router.push({ pathname: '/daemon-editor', params: { id: profile.id } });
              }}
              onSelect={(profile) => void select(profile)}
            />
            <View style={styles.footer}>
              <AppSymbol
                name={{ ios: 'key.horizontal', android: 'key', web: 'key' }}
                size={14}
                tintColor={theme.textTertiary}
              />
              <Text style={[styles.footerText, { color: theme.textTertiary }]}>
                {t('daemon.list_footer')}
              </Text>
            </View>
          </>
        ) : (
          <View style={styles.empty}>
            <Text style={[styles.emptyTitle, { color: theme.text }]}>
              {t('daemon.none_saved')}
            </Text>
            <Text style={[styles.emptyBody, { color: theme.textSecondary }]}>
              {t('daemon.none_saved_body')}
            </Text>
          </View>
        )}
        <Text style={[styles.sectionTitle, { color: theme.textTertiary }]}>
          {t('settings.general')}
        </Text>
        <LanguagePickerRow />
      </ScrollView>
    </View>
  );
}

const styles = StyleSheet.create({
  screen: { flex: 1 },
  listContent: { paddingBottom: 36, paddingHorizontal: 16 },
  sectionTitle: {
    fontSize: 13,
    fontWeight: '500',
    marginBottom: 7,
    marginLeft: 12,
    marginTop: 14,
    textTransform: 'uppercase',
  },
  footer: {
    alignItems: 'flex-start',
    flexDirection: 'row',
    gap: 8,
    marginHorizontal: 12,
    marginTop: 14,
  },
  footerText: { flex: 1, fontSize: 12, lineHeight: 17 },
  empty: { alignItems: 'center', paddingHorizontal: 40, paddingTop: 100 },
  emptyTitle: { fontSize: 18, fontWeight: '700' },
  emptyBody: { fontSize: 14, lineHeight: 20, marginTop: 8, maxWidth: 320, textAlign: 'center' },
});
