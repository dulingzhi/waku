import { ActivityIndicator, FlatList, StyleSheet, Text, TextInput, View } from 'react-native';

import { AppSymbol } from './app-symbol';
import { Sheet, SheetRow } from './sheet';
import { NativeTint, Radius } from '@/constants/theme';
import type { useComposerPicker } from '@/hooks/use-composer-picker';
import { useModelSheetListHeight } from '@/hooks/use-model-sheet-list-height';
import { useTheme } from '@/hooks/use-theme';
import { useI18n } from '@/lib/i18n-context';

export function ComposerContextPicker({
  kind, visible, query, onQueryChange, rows, hasResults, loading, error, onSelect, onDismiss, onRetry,
}: ReturnType<typeof useComposerPicker>['picker']) {
  const theme = useTheme();
  const { t } = useI18n();
  const listHeight = useModelSheetListHeight(visible);
  const commands = kind === 'command';
  return (
    <Sheet visible={visible} onDismiss={onDismiss} scrollable={false} title={commands ? t('composer_ui.commands') : t('composer_ui.mention_files')}>
      <View style={[styles.search, { backgroundColor: theme.inset }]}>
        <TextInput
          accessibilityLabel={commands ? t('composer_ui.search_commands') : t('composer_ui.search_project_files')}
          autoCapitalize="none"
          autoCorrect={false}
          placeholder={commands ? t('composer_ui.search_commands_placeholder') : t('composer_ui.search_files_placeholder')}
          placeholderTextColor={theme.textTertiary}
          selectionColor={NativeTint}
          style={[styles.searchInput, { color: theme.text }]}
          value={query}
          onChangeText={onQueryChange}
        />
        <View style={styles.searchProgress}>
          {loading && <ActivityIndicator accessibilityLabel={t('composer_ui.searching')} color={theme.textTertiary} size="small" />}
        </View>
      </View>
        <FlatList
          data={rows}
          initialNumToRender={8}
          keyboardDismissMode="on-drag"
          keyboardShouldPersistTaps="handled"
          style={{ height: listHeight }}
          keyExtractor={(row) => row.kind === 'command' ? `command:${row.command.name}` : `file:${row.file.path}`}
          ListEmptyComponent={error ? (
            <>
              <Text accessibilityLiveRegion="polite" style={[styles.note, { color: theme.danger }]}>{error}</Text>
              <SheetRow label={t('common.retry')} onPress={onRetry} />
            </>
          ) : (
            <Text style={[styles.note, { color: theme.textTertiary }]}>
              {loading && !hasResults
                ? commands ? t('composer_ui.loading_commands') : t('composer_ui.finding_files')
                : commands ? t('composer_ui.no_matching_commands') : t('composer_ui.no_matching_files')}
            </Text>
          )}
          renderItem={({ item }) => (
            <SheetRow
              label={item.kind === 'command' ? `/${item.command.name}` : item.file.path}
              description={item.kind === 'command'
                ? [item.command.scope.toLowerCase(), item.command.argument_hint, item.command.description].filter(Boolean).join(' · ')
                : item.file.is_dir ? t('composer_ui.folder') : t('composer_ui.file')}
              leading={item.kind === 'file' ? (
                <AppSymbol
                  name={item.file.is_dir
                    ? { ios: 'folder', android: 'folder', web: 'folder' }
                    : { ios: 'doc', android: 'description', web: 'description' }}
                  size={17}
                  tintColor={theme.textTertiary}
                />
              ) : undefined}
              onPress={() => onSelect(item)}
            />
          )}
        />
    </Sheet>
  );
}

const styles = StyleSheet.create({
  search: { borderRadius: Radius.medium, flexDirection: 'row', alignItems: 'center', paddingLeft: 12, paddingRight: 8, marginHorizontal: 6, marginBottom: 8 },
  searchInput: { flex: 1, fontSize: 15, minHeight: 44, paddingVertical: 10 },
  searchProgress: { width: 28, alignItems: 'center', justifyContent: 'center' },
  note: { fontSize: 13, lineHeight: 18, paddingHorizontal: 12, paddingVertical: 14 },
});
