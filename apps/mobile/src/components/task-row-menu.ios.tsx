import { Button, Host, Menu, RNHostView } from '@expo/ui/swift-ui';
import {
  accessibilityAddTraits,
  accessibilityHint,
  accessibilityLabel,
} from '@expo/ui/swift-ui/modifiers';

import type { TaskRowMenuProps } from '@/components/task-row-menu.types';
import { useI18n } from '@/lib/i18n-context';

/**
 * SwiftUI Menu's primary action fires a normal tap directly while reserving
 * long-press for the native menu. ContextMenu delays its hosted React Native
 * child's tap while it arbitrates the long-press gesture.
 */
export function TaskRowMenu({
  accessibilityLabel: label,
  onDelete,
  onRename,
  onSelect,
  renderTrigger,
  selected,
  style,
}: TaskRowMenuProps) {
  const { t } = useI18n();
  return (
    <Host ignoreSafeArea="all" matchContents style={style}>
      <Menu
        label={<RNHostView matchContents>{renderTrigger(false)}</RNHostView>}
        modifiers={[
          accessibilityLabel(label),
          accessibilityHint(t('task.row_actions_hint')),
          accessibilityAddTraits(selected ? ['isButton', 'isSelected'] : ['isButton']),
        ]}
        onPrimaryAction={onSelect}>
        <Button label={t('task.rename')} systemImage="pencil" onPress={onRename} />
        <Button
          label={t('task.delete')}
          role="destructive"
          systemImage="trash"
          onPress={onDelete}
        />
      </Menu>
    </Host>
  );
}
