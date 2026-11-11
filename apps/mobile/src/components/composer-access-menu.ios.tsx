import type { RuntimeMode } from '@waku/client';
import * as Haptics from 'expo-haptics';
import {
  Host,
  Label,
  Menu,
  RNHostView,
  Section,
  Text as SwiftUIText,
  Toggle,
  VStack,
} from '@expo/ui/swift-ui';
import {
  accessibilityAddTraits,
  accessibilityLabel,
  font,
  foregroundStyle,
  lineLimit,
} from '@expo/ui/swift-ui/modifiers';
import { StyleSheet, View } from 'react-native';

import { AppSymbol } from './app-symbol';
import type { ComposerAccessMenuProps } from './composer-access-menu.types';
import { Radius } from '@/constants/theme';
import { useTheme } from '@/hooks/use-theme';
import { useI18n } from '@/lib/i18n-context';
import { runtimeModeLabel } from '@/lib/session-presentation';

const ACCESS_MODES: Array<{
  id: RuntimeMode;
  descriptionKey: string;
  icon: 'lock' | 'pencil' | 'sparkles' | 'lock.open';
}> = [
  { id: 'ask', descriptionKey: 'composer_ui.approve_everything', icon: 'lock' },
  {
    id: 'autoAcceptEdits',
    descriptionKey: 'mode.auto_accept_edits_description',
    icon: 'pencil',
  },
  { id: 'auto', descriptionKey: 'composer_ui.works_autonomously', icon: 'sparkles' },
  {
    id: 'fullAccess',
    descriptionKey: 'composer_ui.no_approval_prompts',
    icon: 'lock.open',
  },
];

/** The task-page access control uses SwiftUI Menu so it stays attached to the
 * composer trigger and receives the native popover arrow and dismissal model. */
export function ComposerAccessMenu({ mode, onApply }: ComposerAccessMenuProps) {
  const theme = useTheme();
  const { t } = useI18n();
  const selected = ACCESS_MODES.find((item) => item.id === mode) ?? ACCESS_MODES[3]!;

  return (
    <Host ignoreSafeArea="all" matchContents>
      <Menu
        label={(
          <RNHostView matchContents>
            <View accessible={false} style={styles.trigger}>
              <AppSymbol name={selected.icon} size={19} tintColor={theme.textSecondary} />
            </View>
          </RNHostView>
        )}
        modifiers={[
          accessibilityLabel(`${t('composer_ui.agent_access')}, ${runtimeModeLabel(mode, t)}`),
          accessibilityAddTraits(['isButton']),
        ]}>
        <Section title={t('composer_ui.agent_access')}>
          {ACCESS_MODES.map((item) => (
            <Toggle
              isOn={item.id === mode}
              key={item.id}
              onIsOnChange={() => {
                void Haptics.selectionAsync();
                onApply(item.id);
              }}>
              <Label systemImage={item.icon}>
                <VStack alignment="leading" spacing={1}>
                  <SwiftUIText>{runtimeModeLabel(item.id, t)}</SwiftUIText>
                  <SwiftUIText
                    modifiers={[
                      font({ textStyle: 'caption' }),
                      foregroundStyle({ type: 'hierarchical', style: 'secondary' }),
                      lineLimit(2),
                    ]}>
                    {t(item.descriptionKey)}
                  </SwiftUIText>
                </VStack>
              </Label>
            </Toggle>
          ))}
        </Section>
      </Menu>
    </Host>
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
