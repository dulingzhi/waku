import { MenuView } from '@expo/ui/community/menu';
import { StyleSheet, View } from 'react-native';

import { AppSymbol } from './app-symbol';
import { Radius } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useI18n } from '@/lib/i18n-context';

export type ComposerAttachmentSource = 'files' | 'camera' | 'photo';

export function ComposerAddMenu({
  disabled = false,
  onChoose,
  onChooseContext,
}: {
  disabled?: boolean;
  onChoose?: (source: ComposerAttachmentSource) => void;
  onChooseContext: (kind: 'command' | 'file') => void;
}) {
  const theme = useTheme();
  const { t } = useI18n();
  const trigger = (
    <View
      accessible
      accessibilityLabel={t('composer_ui.add_to_message')}
      accessibilityRole="button"
      accessibilityState={{ disabled }}
      style={[styles.trigger, { opacity: disabled ? 0.35 : 1 }]}>
      <AppSymbol
        name={{ ios: 'plus', android: 'add', web: 'add' }}
        size={20}
        tintColor={theme.textSecondary}
      />
    </View>
  );
  if (disabled) return trigger;

  return (
    <MenuView
      actions={[
        { id: 'command', title: t('composer_ui.commands'), image: 'command' },
        { id: 'mention', title: t('composer_ui.mention_files'), image: 'at' },
        ...(onChoose ? [
          { id: 'files', title: t('composer_ui.files'), image: 'folder' as const },
          { id: 'camera', title: t('composer_ui.camera'), image: 'camera' as const },
          { id: 'photo', title: t('composer_ui.photo'), image: 'photo' as const },
        ] : []),
      ]}
      onPressAction={({ nativeEvent }) => {
        const source = nativeEvent.event;
        // Let the native menu finish dismissing before presenting a picker.
        setTimeout(() => {
          if (source === 'command' || source === 'mention') onChooseContext(source === 'command' ? 'command' : 'file');
          else if (source === 'files' || source === 'camera' || source === 'photo') onChoose?.(source);
        }, 160);
      }}>
      {trigger}
    </MenuView>
  );
}

const styles = StyleSheet.create({
  trigger: {
    alignItems: 'center',
    borderRadius: Radius.pill,
    height: 36,
    justifyContent: 'center',
    width: 36,
  },
});
