import { useState } from 'react';

import { AppSymbol } from '@/components/app-symbol';
import { Sheet, SheetRow } from '@/components/sheet';
import { useI18n } from '@/lib/i18n-context';
import { APP_LANGUAGES, type AppLanguage } from '@/lib/i18n';
import { NativeTint } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { Pressable, StyleSheet, Text, View } from 'react-native';

/**
 * Language selector. The values are autonyms and the `system` row resolves to
 * one of the shipped locales, matching the desktop and web selectors.
 */
export function LanguagePickerRow() {
  const { language, languageLabel, setLanguage, t } = useI18n();
  const theme = useTheme();
  const [open, setOpen] = useState(false);

  return (
    <>
      <Pressable
        accessibilityHint={t('language.description')}
        accessibilityLabel={t('language_picker.title')}
        accessibilityRole="button"
        onPress={() => setOpen(true)}
        style={({ pressed }) => [
          styles.row,
          { backgroundColor: pressed ? theme.overlay : 'transparent' },
        ]}>
        <AppSymbol
          name={{ ios: 'globe', android: 'language', web: 'language' }}
          size={15}
          tintColor={theme.textTertiary}
        />
        <Text style={[styles.label, { color: theme.text }]}>{t('language_picker.title')}</Text>
        <Text numberOfLines={1} style={[styles.value, { color: theme.textTertiary }]}>
          {languageLabel(language)}
        </Text>
      </Pressable>
      <Sheet
        onDismiss={() => setOpen(false)}
        title={t('language_picker.title')}
        visible={open}>
        {APP_LANGUAGES.map((option) => (
          <SheetRow
            key={option}
            label={languageLabel(option)}
            selected={option === language}
            onPress={() => {
              setLanguage(option as AppLanguage);
              setOpen(false);
            }}
          />
        ))}
      </Sheet>
    </>
  );
}

const styles = StyleSheet.create({
  row: {
    alignItems: 'center',
    borderRadius: 10,
    flexDirection: 'row',
    gap: 10,
    marginHorizontal: 12,
    paddingHorizontal: 12,
    paddingVertical: 12,
  },
  label: { flex: 1, fontSize: 15, fontWeight: '500' },
  value: { fontSize: 15, maxWidth: 160 },
  selectedTint: { color: NativeTint },
});
