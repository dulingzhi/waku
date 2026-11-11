import { MenuView } from '@expo/ui/community/menu';
import { useMemo } from 'react';
import { Pressable } from 'react-native';

import type { TaskRowMenuProps } from '@/components/task-row-menu.types';
import { useI18n } from '@/lib/i18n-context';

export function TaskRowMenu({
  accessibilityLabel,
  onDelete,
  onRename,
  onSelect,
  renderTrigger,
  selected,
  style,
}: TaskRowMenuProps) {
  const { t } = useI18n();
  const actions = useMemo(() => [
    { id: 'rename', title: t('task.rename'), image: 'pencil' as const },
    {
      id: 'delete',
      title: t('task.delete'),
      image: 'trash' as const,
      attributes: { destructive: true as const },
    },
  ], [t]);

  return (
    <MenuView
      actions={actions}
      onPressAction={({ nativeEvent }) => {
        if (nativeEvent.event === 'rename') onRename();
        else if (nativeEvent.event === 'delete') onDelete();
      }}
      shouldOpenOnLongPress
      style={style}>
      <Pressable
        accessibilityHint={t('task.row_actions_hint')}
        accessibilityLabel={accessibilityLabel}
        accessibilityRole="button"
        accessibilityState={{ selected }}
        onPress={onSelect}>
        {({ pressed }) => renderTrigger(pressed)}
      </Pressable>
    </MenuView>
  );
}
