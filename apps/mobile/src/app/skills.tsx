import { Stack, router, useFocusEffect } from 'expo-router';
import { useCallback, useMemo, useRef, useState } from 'react';
import {
  ActivityIndicator,
  Pressable,
  SectionList,
  StyleSheet,
  Text,
  TextInput,
  View,
} from 'react-native';

import { AppSymbol } from '@/components/app-symbol';
import { ConnectionBanner } from '@/components/connection-banner';
import { ProviderIcon } from '@/components/provider-icon';
import { Radius, Spacing } from '@/constants/theme';
import { useSkills, useTaskState } from '@/hooks/use-daemon-data';
import { useTheme } from '@/hooks/use-theme';
import type { Project, ProviderKind, SkillEntry } from '@waku/client';
import { useDaemon } from '@/lib/daemon-context';
import { useI18n } from '@/lib/i18n-context';
import {
  groupSkillRows,
  skillMatches,
  skillRouteKey,
  skillSourcesLabel,
  skillsCountLabel,
  skillSourceProviders,
  type SkillListRow,
} from '@/lib/skills-presentation';

/**
 * The daemon's skill library. Skills live on the daemon host, so this lists
 * what that machine has — including project-scoped skills for every project
 * the daemon knows about, which is the whole point of reaching it from a
 * phone.
 *
 * Read-only for now; enabling, disabling and deleting come next.
 */
export default function SkillsScreen() {
  const theme = useTheme();
  const { t } = useI18n();
  const daemon = useDaemon();
  const taskState = useTaskState();
  const projects = useMemo<Project[]>(() => taskState.data?.projects ?? [], [taskState.data]);
  const skills = useSkills(projects);
  const [query, setQuery] = useState('');

  const catalog = useMemo(() => skills.data?.skills ?? [], [skills.data]);
  const matches = useMemo(
    () => catalog.filter((skill) => skillMatches(skill, query)),
    [catalog, query],
  );
  const rows = useMemo(() => groupSkillRows(matches, t), [matches, t]);
  const sections = useMemo(() => toSections(rows), [rows]);

  const connected = daemon.phase === 'connected';
  const loading = skills.isPending && connected;
  const noMatch = !loading && catalog.length > 0 && matches.length === 0;

  // Opening a detail route unmounts nothing, but the list is re-created on a
  // cold return in some navigator paths — so the row the user tapped is
  // remembered and scrolled back into view.
  //
  // Keyed on `skillRouteKey`, not on the row's React key: `rowKey` is part of
  // that key and the daemon re-hashes it whenever a skill is toggled, so a row
  // key remembered before a toggle no longer matches after it and the scroll
  // restore would silently give up. `(project, name)` survives the toggle.
  const listRef = useRef<SectionList<SkillRowItem, SkillSection> | null>(null);
  const lastRouteKeyRef = useRef<string | null>(null);
  useFocusEffect(useCallback(() => {
    const target = lastRouteKeyRef.current;
    if (!target) return;
    const sectionIndex = sections.findIndex((section) => (
      section.data.some((item) => skillRouteKey(item.skill) === target)
    ));
    if (sectionIndex < 0) return;
    const itemIndex = sections[sectionIndex]!.data.findIndex(
      (item) => skillRouteKey(item.skill) === target,
    );
    // A frame after focus, so a reflow from the returning transition does not
    // land the scroll in the old position.
    const frame = requestAnimationFrame(() => {
      listRef.current?.scrollToLocation({
        animated: false,
        itemIndex: Math.max(0, itemIndex),
        sectionIndex,
        viewOffset: 12,
      });
    });
    return () => cancelAnimationFrame(frame);
  }, [sections]));

  return (
    <View style={[styles.screen, { backgroundColor: theme.background }]}>
      <Stack.Screen options={{ title: t('skills.library') }} />
      <SectionList<SkillRowItem, SkillSection>
        ref={listRef}
        contentInsetAdjustmentBehavior="automatic"
        contentContainerStyle={styles.listContent}
        keyboardShouldPersistTaps="handled"
        sections={sections}
        showsVerticalScrollIndicator={false}
        stickySectionHeadersEnabled={false}
        keyExtractor={(item) => item.key}
        getItemLayout={undefined}
        ListHeaderComponent={(
          <View style={styles.header}>
            <ConnectionBanner />
            {connected && catalog.length > 0 && (
              <>
                <SearchField onChange={setQuery} value={query} />
                <Text style={[styles.count, { color: theme.textTertiary }]}>
                  {query.trim()
                    ? t('skills.filter_caption', { shown: matches.length, total: catalog.length })
                    : skillsCountLabel(catalog, t)}
                </Text>
              </>
            )}
          </View>
        )}
        renderSectionHeader={({ section }) => (
          <Text style={[styles.sectionTitle, { color: theme.textTertiary }]}>
            {section.title}
          </Text>
        )}
        renderItem={({ item }) => (
          <SkillRow
            skill={item.skill}
            skillSources={skillSourcesLabel(item.skill, t)}
            providers={skillSourceProviders(item.skill)}
            onPress={() => {
              lastRouteKeyRef.current = skillRouteKey(item.skill);
              router.push({
                pathname: '/skills/[key]',
                params: { key: skillRouteKey(item.skill) },
              });
            }}
          />
        )}
        ListEmptyComponent={(
          <SkillsEmpty
            error={skills.error}
            loading={loading}
            noMatch={noMatch}
            onRetry={() => void skills.refetch()}
            query={query}
          />
        )}
      />
    </View>
  );
}

type SkillRowItem = Extract<SkillListRow, { type: 'skill' }>;

type SkillSection = {
  title: string;
  key: string;
  data: SkillRowItem[];
};

function toSections(rows: readonly SkillListRow[]): SkillSection[] {
  const sections: SkillSection[] = [];
  for (const row of rows) {
    if (row.type === 'section') {
      sections.push({ title: `${row.label} · ${row.count}`, key: row.key, data: [] });
    } else {
      sections.at(-1)?.data.push(row);
    }
  }
  return sections;
}

function SearchField({ value, onChange }: { value: string; onChange: (next: string) => void }) {
  const theme = useTheme();
  const { t } = useI18n();
  return (
    <View style={[styles.search, { backgroundColor: theme.inset, borderColor: theme.border }]}>
      <AppSymbol
        name={{ ios: 'magnifyingglass', android: 'search', web: 'search' }}
        size={14}
        tintColor={theme.textTertiary}
      />
      <TextInput
        accessibilityLabel={t('skills.search')}
        autoCapitalize="none"
        autoCorrect={false}
        placeholder={t('skills.search')}
        placeholderTextColor={theme.textGhost}
        selectionColor={theme.accent}
        style={[styles.searchInput, { color: theme.text }]}
        value={value}
        onChangeText={onChange}
      />
      {value.length > 0 && (
        <Pressable
          accessibilityLabel={t('common.cancel')}
          accessibilityRole="button"
          hitSlop={8}
          onPress={() => onChange('')}>
          <AppSymbol
            name={{ ios: 'xmark.circle.fill', android: 'cancel', web: 'cancel' }}
            size={15}
            tintColor={theme.textTertiary}
          />
        </Pressable>
      )}
    </View>
  );
}

function SkillRow({
  skill,
  skillSources,
  providers,
  onPress,
}: {
  skill: SkillEntry;
  skillSources: string;
  providers: ProviderKind[];
  onPress: () => void;
}) {
  const theme = useTheme();
  const { t } = useI18n();
  return (
    <Pressable
      accessibilityHint={t('skills.details')}
      accessibilityRole="button"
      onPress={onPress}
      style={({ pressed }) => [
        styles.row,
        { backgroundColor: theme.surface, borderColor: theme.border },
        !skill.enabled && styles.rowDisabled,
        pressed && styles.rowPressed,
      ]}>
      <View style={styles.rowHead}>
        <Text
          numberOfLines={1}
          style={[styles.rowName, { color: skill.enabled ? theme.text : theme.textTertiary }]}>
          {skill.name}
        </Text>
        {providers.length > 0 && (
          <View style={styles.rowProviders}>
            {providers.map((provider) => (
              <ProviderIcon
                key={provider}
                provider={provider}
                size={13}
                // A disabled skill must not read as active through its marks.
                color={skill.enabled ? undefined : theme.textGhost}
              />
            ))}
          </View>
        )}
        {!skill.enabled && (
          <View style={[styles.badge, { backgroundColor: theme.overlayStrong }]}>
            <Text style={[styles.badgeText, { color: theme.textTertiary }]}>
              {t('skills.disabled_badge')}
            </Text>
          </View>
        )}
      </View>
      <Text
        numberOfLines={2}
        style={[styles.rowDescription, { color: theme.textSecondary }]}>
        {skill.description || t('skills.no_description')}
      </Text>
      <Text numberOfLines={1} style={[styles.rowMeta, { color: theme.textGhost }]}>
        {skillSources}
      </Text>
    </Pressable>
  );
}

function SkillsEmpty({
  error,
  loading,
  noMatch,
  query,
  onRetry,
}: {
  error: unknown;
  loading: boolean;
  noMatch: boolean;
  query: string;
  onRetry: () => void;
}) {
  const theme = useTheme();
  const { t } = useI18n();
  if (loading) {
    return (
      <View style={styles.empty}>
        <ActivityIndicator />
        <Text style={[styles.emptyBody, { color: theme.textTertiary }]}>
          {t('skills.scanning')}
        </Text>
      </View>
    );
  }
  if (error) {
    return (
      <View style={styles.empty}>
        <Text style={[styles.emptyTitle, { color: theme.text }]}>
          {t('skills.empty_title')}
        </Text>
        <Text style={[styles.emptyBody, { color: theme.textSecondary }]}>
          {error instanceof Error ? error.message : String(error)}
        </Text>
        <Pressable
          accessibilityRole="button"
          onPress={onRetry}
          style={({ pressed }) => [
            styles.retry,
            { borderColor: theme.borderStrong, opacity: pressed ? 0.6 : 1 },
          ]}>
          <Text style={[styles.retryText, { color: theme.text }]}>{t('common.retry')}</Text>
        </Pressable>
      </View>
    );
  }
  return (
    <View style={styles.empty}>
      <Text style={[styles.emptyTitle, { color: theme.text }]}>
        {noMatch ? t('skills.no_match') : t('skills.empty_title')}
      </Text>
      {!noMatch && (
        <Text style={[styles.emptyBody, { color: theme.textSecondary }]}>
          {t('skills.empty_description')}
        </Text>
      )}
      {noMatch && query.trim() ? (
        <Text style={[styles.emptyBody, { color: theme.textSecondary }]}>{query.trim()}</Text>
      ) : null}
    </View>
  );
}

const styles = StyleSheet.create({
  screen: { flex: 1 },
  listContent: { paddingBottom: 36, paddingHorizontal: Spacing.three },
  header: { gap: 12, paddingTop: 6 },
  search: {
    alignItems: 'center',
    borderRadius: Radius.small,
    borderWidth: StyleSheet.hairlineWidth,
    flexDirection: 'row',
    gap: 7,
    paddingHorizontal: 10,
    paddingVertical: 8,
  },
  searchInput: { flex: 1, fontSize: 14, minHeight: 20, padding: 0 },
  count: { fontSize: 12, marginBottom: 2, marginTop: -4 },
  sectionTitle: {
    fontSize: 12,
    fontWeight: '600',
    letterSpacing: 0.4,
    marginBottom: 6,
    marginTop: 14,
    textTransform: 'uppercase',
  },
  row: {
    borderRadius: Radius.medium,
    borderWidth: StyleSheet.hairlineWidth,
    gap: 5,
    marginBottom: 8,
    paddingHorizontal: 13,
    paddingVertical: 11,
  },
  rowDisabled: { opacity: 0.72 },
  rowPressed: { opacity: 0.6 },
  rowHead: { alignItems: 'center', flexDirection: 'row', gap: 8 },
  rowName: { flexShrink: 1, fontSize: 15, fontWeight: '600' },
  rowProviders: { alignItems: 'center', flexDirection: 'row', gap: 5 },
  badge: { borderRadius: Radius.pill, paddingHorizontal: 7, paddingVertical: 2 },
  badgeText: { fontSize: 10.5, fontWeight: '600' },
  rowDescription: { fontSize: 13, lineHeight: 18 },
  rowMeta: { fontSize: 11 },
  empty: { alignItems: 'center', gap: 10, paddingHorizontal: 32, paddingTop: 80 },
  emptyTitle: { fontSize: 17, fontWeight: '700', textAlign: 'center' },
  emptyBody: { fontSize: 13.5, lineHeight: 19, maxWidth: 320, textAlign: 'center' },
  retry: {
    borderRadius: Radius.small,
    borderWidth: StyleSheet.hairlineWidth,
    marginTop: 4,
    paddingHorizontal: 14,
    paddingVertical: 7,
  },
  retryText: { fontSize: 13.5, fontWeight: '500' },
});
