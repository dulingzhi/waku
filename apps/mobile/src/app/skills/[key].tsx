import type { SkillEntry, SkillsCatalog } from '@waku/client';
import { Stack, router, useLocalSearchParams } from 'expo-router';
import { useQueryClient } from '@tanstack/react-query';
import { useCallback, useMemo, useRef, useState } from 'react';
import {
  ActivityIndicator,
  Alert,
  Pressable,
  ScrollView,
  StyleSheet,
  Switch,
  Text,
  View,
} from 'react-native';

import { AppSymbol } from '@/components/app-symbol';
import { ProviderIcon } from '@/components/provider-icon';
import { Radius, Spacing } from '@/constants/theme';
import { useSkills, useTaskState } from '@/hooks/use-daemon-data';
import { useTheme } from '@/hooks/use-theme';
import { daemonKeys, setSkillsEnabled, trashSkills } from '@/lib/daemon-api';
import { useDaemon } from '@/lib/daemon-context';
import { useI18n } from '@/lib/i18n-context';
import { MarkdownDocument } from '@/md/document';
import {
  findSkillByRouteKey,
  formatSkillBytes,
  formatSkillUpdated,
  skillDuplicatesLabel,
  skillFilesLabel,
  skillInstallCountLabel,
  skillRouteKey,
  skillScopeLabel,
  skillSourceProviders,
  skillSourceLabel,
  skillSourcesLabel,
  visibleSkillInstalls,
  withSkillEnabled,
} from '@/lib/skills-presentation';

/**
 * One skill's detail pane: its metadata, then its SKILL.md rendered as a
 * static Markdown document.
 *
 * A route rather than a sheet because SKILL.md is a long document and it has
 * to scroll; a sheet gives it a short viewport and a second scroll surface.
 */
export default function SkillDetailScreen() {
  const params = useLocalSearchParams<{ key?: string | string[] }>();
  const key = Array.isArray(params.key) ? params.key[0] : params.key;
  const theme = useTheme();
  const { t } = useI18n();
  const daemon = useDaemon();
  const queryClient = useQueryClient();
  const taskState = useTaskState();
  const projects = useMemo(() => taskState.data?.projects ?? [], [taskState.data]);
  const skills = useSkills(projects);
  const catalog = useMemo(() => skills.data?.skills ?? [], [skills.data]);
  const skill = key ? findSkillByRouteKey(catalog, key) : undefined;

  /** One mutation per session at a time: a toggle mid-delete (or a second
   * delete) must not reach the daemon as two contradictory commands. */
  const inFlight = useRef(false);
  const [pending, setPending] = useState<'toggle' | 'delete' | null>(null);
  const [deleteArmed, setDeleteArmed] = useState(false);

  const refresh = useCallback(async () => {
    const profileId = daemon.activeProfile?.id;
    if (!profileId) return;
    await queryClient.invalidateQueries({ queryKey: daemonKeys.skills(profileId) });
  }, [daemon.activeProfile?.id, queryClient]);

  const toggle = useCallback(async (enabled: boolean) => {
    const client = daemon.client;
    const profileId = daemon.activeProfile?.id;
    if (!client || !profileId || inFlight.current || !skill) return;
    const dirs = skill.installs.map((install) => install.dir);
    if (!dirs.length) return;
    inFlight.current = true;
    setPending('toggle');
    // The switch answers immediately, like the desktop's; the rescan confirms
    // from disk. A failure reverts by invalidating, not by guessing.
    queryClient.setQueryData<{ skills: SkillEntry[] }>(
      daemonKeys.skills(profileId),
      (current) => current
        ? withSkillEnabled(current, skillRouteKey(skill), enabled)
        : current,
    );
    try {
      await setSkillsEnabled(client, dirs, enabled);
      setDeleteArmed(false);
    } catch (cause) {
      Alert.alert(t('skills.toggle_failed', {
        error: cause instanceof Error ? cause.message : String(cause),
      }));
    } finally {
      inFlight.current = false;
      setPending(null);
      await refresh().catch(() => {});
    }
  }, [daemon.client, daemon.activeProfile?.id, queryClient, refresh, skill, t]);

  const remove = useCallback(async () => {
    const client = daemon.client;
    const profileId = daemon.activeProfile?.id;
    if (!client || !profileId || inFlight.current || !skill) return;
    const dirs = skill.installs.map((install) => install.dir);
    if (!dirs.length) return;
    const routeKey = skillRouteKey(skill);
    inFlight.current = true;
    setPending('delete');
    try {
      await trashSkills(client, dirs);
      // Drop the row from the cache *before* popping: the list is still
      // mounted underneath, and invalidating alone would let the popped-to
      // list paint the deleted skill for the frame the refetch takes.
      queryClient.setQueryData<SkillsCatalog>(
        daemonKeys.skills(profileId),
        (current) => current
          ? { ...current, skills: current.skills.filter((item) => skillRouteKey(item) !== routeKey) }
          : current,
      );
      router.back();
      // The refetch then supplies the daemon's own view of what is left, so a
      // stale entry cannot survive a failed optimistic edit above.
      await refresh().catch(() => {});
    } catch (cause) {
      Alert.alert(t('skills.delete_failed', {
        error: cause instanceof Error ? cause.message : String(cause),
      }));
    } finally {
      inFlight.current = false;
      setPending(null);
    }
  }, [daemon.client, daemon.activeProfile?.id, queryClient, refresh, skill, t]);

  if (skills.isPending) {
    return (
      <View style={[styles.centered, { backgroundColor: theme.background }]}>
        <ActivityIndicator />
        <Text style={[styles.hint, { color: theme.textTertiary }]}>{t('skills.scanning')}</Text>
      </View>
    );
  }

  if (!skill) {
    return (
      <View style={[styles.centered, { backgroundColor: theme.background }]}>
        <Text style={[styles.missingTitle, { color: theme.text }]}>
          {t('skills.select_placeholder')}
        </Text>
      </View>
    );
  }

  return (
    <View style={[styles.screen, { backgroundColor: theme.background }]}>
      <Stack.Screen options={{ title: skill.name }} />
      <ScrollView
        contentInsetAdjustmentBehavior="automatic"
        contentContainerStyle={styles.content}
        showsVerticalScrollIndicator={false}>
        <SkillHeader
          busy={pending === 'toggle'}
          skill={skill}
          onToggle={(enabled) => void toggle(enabled)}
        />
        <SkillMetadata skill={skill} />
        <SkillActions
          armed={deleteArmed}
          busy={pending === 'delete'}
          disabled={pending !== null}
          installCount={skill.installs.length}
          onArm={() => setDeleteArmed(true)}
          onDisarm={() => setDeleteArmed(false)}
          onDelete={() => void remove()}
        />
        {skill.body.trim() ? (
          <View style={styles.contents}>
            <Text style={[styles.sectionTitle, { color: theme.textTertiary }]}>
              {t('skills.detail_contents')}
            </Text>
            <MarkdownDocument source={skill.body} />
          </View>
        ) : null}
      </ScrollView>
    </View>
  );
}

function SkillHeader({
  skill,
  busy,
  onToggle,
}: {
  skill: SkillEntry;
  busy: boolean;
  onToggle: (enabled: boolean) => void;
}) {
  const theme = useTheme();
  const { t } = useI18n();
  const providers = skillSourceProviders(skill);
  return (
    <View style={styles.header}>
      <View style={styles.headerLine}>
        <Text style={[styles.name, { color: theme.text }]}>{skill.name}</Text>
        {providers.map((provider) => (
          <ProviderIcon key={provider} provider={provider} size={16} />
        ))}
        {!skill.enabled && (
          <View style={[styles.badge, { backgroundColor: theme.overlayStrong }]}>
            <Text style={[styles.badgeText, { color: theme.textTertiary }]}>
              {t('skills.disabled_badge')}
            </Text>
          </View>
        )}
        <View style={styles.headerSpacer} />
        {busy ? (
          <ActivityIndicator size="small" />
        ) : (
          <Switch
            accessibilityLabel={t(skill.enabled ? 'skills.disable_named' : 'skills.enable_named', {
              name: skill.name,
            })}
            onValueChange={onToggle}
            trackColor={{ false: theme.inset, true: theme.inverse }}
            value={skill.enabled}
          />
        )}
      </View>
      {skill.description ? (
        <Text style={[styles.description, { color: theme.textSecondary }]}>
          {skill.description}
        </Text>
      ) : (
        <Text style={[styles.description, { color: theme.textTertiary }]}>
          {t('skills.no_description')}
        </Text>
      )}
      <View style={styles.scopeLine}>
        <AppSymbol
          name={{ ios: 'scope', android: 'track_changes', web: 'track_changes' }}
          size={12}
          tintColor={theme.textGhost}
        />
        <Text style={[styles.scopeText, { color: theme.textTertiary }]}>
          {skillSourcesLabel(skill, t)} · {skillScopeLabel(skill, t)}
        </Text>
      </View>
    </View>
  );
}

/**
 * Delete, in two steps: the first tap turns the button into a confirmation,
 * the second trashes the skill. Desktop (`skills_page.rs`, `skills_delete_arming`)
 * and web (`skills-settings.tsx`, `deleteArmed`) both work this way, so mobile
 * follows rather than introducing a fourth idiom — this one needs no dialog
 * because the second tap *is* the confirmation.
 *
 * Every install location goes at once: the list aggregates a skill installed
 * into several ecosystems into one row, so the user means the one thing they
 * see, not one directory of it. The button says how many that is.
 */
function SkillActions({
  installCount,
  armed,
  busy,
  disabled,
  onArm,
  onDisarm,
  onDelete,
}: {
  installCount: number;
  armed: boolean;
  busy: boolean;
  disabled: boolean;
  onArm: () => void;
  onDisarm: () => void;
  onDelete: () => void;
}) {
  const theme = useTheme();
  const { t } = useI18n();
  return (
    <View style={styles.actions}>
      <Pressable
        accessibilityRole="button"
        accessibilityState={{ disabled: disabled || busy }}
        disabled={disabled || busy}
        onBlur={onDisarm}
        onPress={armed ? onDelete : onArm}
        style={({ pressed }) => [
          styles.deleteButton,
          {
            borderColor: armed ? theme.danger : theme.borderStrong,
            opacity: pressed || busy ? 0.6 : 1,
          },
        ]}>
        {busy
          ? <ActivityIndicator size="small" />
          : (
              <AppSymbol
                name={{ ios: 'trash', android: 'delete', web: 'delete' }}
                size={13}
                tintColor={armed ? theme.danger : theme.textSecondary}
              />
            )}
        <Text style={[styles.deleteText, { color: armed ? theme.danger : theme.text }]}>
          {t(armed ? 'skills.confirm_delete' : 'skills.delete')}
        </Text>
      </Pressable>
      {armed && (
        <Pressable
          accessibilityRole="button"
          hitSlop={8}
          onPress={onDisarm}
          style={({ pressed }) => [styles.cancelButton, { opacity: pressed ? 0.6 : 1 }]}>
          <Text style={[styles.cancelText, { color: theme.textTertiary }]}>
            {t('common.cancel')}
          </Text>
        </Pressable>
      )}
      <View style={styles.actionsSpacer} />
      {installCount > 1 && (
        <Text style={[styles.installNote, { color: armed ? theme.danger : theme.textGhost }]}>
          {skillInstallCountLabel(installCount, t)}
        </Text>
      )}
    </View>
  );
}

/**
 * Metadata rows. Desktop lists every field it has; a phone cannot hold that
 * many lines without burying the document, so this keeps the four that decide
 * whether a skill is the right one — where it lives, what it costs on disk,
 * when it changed, how much goes with it — and folds the two that are
 * warnings rather than facts (duplicates, allowed tools) into one notice each.
 */
function SkillMetadata({ skill }: { skill: SkillEntry }) {
  const theme = useTheme();
  const { t } = useI18n();
  const updated = formatSkillUpdated(skill.modifiedAt, Math.floor(Date.now() / 1_000), t);
  const duplicates = skillDuplicatesLabel(skill, t);
  // One row per install when there are several, so each location keeps its
  // own source label; capped because a skill copied into every ecosystem
  // would otherwise push the document off the screen.
  const { shown, hidden } = visibleSkillInstalls(skill);
  const multi = skill.installs.length > 1;
  return (
    <View style={[styles.meta, { backgroundColor: theme.surface, borderColor: theme.border }]}>
      {shown.map((dir, index) => (
        <MetaRow
          key={dir}
          label={multi
            ? skillSourceLabel(skill.installs[index]!.source, t)
            : t('skills.detail_location')}
          value={dir}
        />
      ))}
      {hidden > 0 && (
        <MetaRow label="" value={`+${hidden} more`} />
      )}
      <MetaRow label={t('skills.detail_updated')} value={updated ?? '—'} />
      <MetaRow
        label={t('skills.detail_contents')}
        value={`${skillFilesLabel(skill, t)} · ${formatSkillBytes(skill.totalBytes)}`}
      />
      {skill.allowedTools ? (
        <MetaRow label={t('skills.allowed_tools')} value={skill.allowedTools} />
      ) : null}
      {duplicates ? (
        <View style={styles.notice}>
          <AppSymbol
            name={{ ios: 'exclamationmark.triangle', android: 'warning', web: 'warning' }}
            size={12}
            tintColor={theme.warning}
          />
          <Text style={[styles.noticeText, { color: theme.warning }]}>{duplicates}</Text>
        </View>
      ) : null}
    </View>
  );
}

function MetaRow({ label, value }: { label: string; value: string }) {
  const theme = useTheme();
  return (
    <View style={[styles.metaRow, { borderBottomColor: theme.separator }]}>
      <Text style={[styles.metaLabel, { color: theme.textTertiary }]}>{label}</Text>
      <Text selectable style={[styles.metaValue, { color: theme.text }]}>{value}</Text>
    </View>
  );
}

const styles = StyleSheet.create({
  screen: { flex: 1 },
  centered: { alignItems: 'center', flex: 1, gap: 10, justifyContent: 'center', padding: 32 },
  hint: { fontSize: 13 },
  missingTitle: { fontSize: 16, fontWeight: '600' },
  content: { gap: 16, paddingBottom: 40, paddingHorizontal: Spacing.three, paddingTop: 10 },
  header: { gap: 7 },
  headerLine: { alignItems: 'center', flexDirection: 'row', flexWrap: 'wrap', gap: 8 },
  headerSpacer: { flex: 1 },
  name: { fontSize: 22, fontWeight: '700' },
  badge: { borderRadius: Radius.pill, paddingHorizontal: 7, paddingVertical: 2 },
  badgeText: { fontSize: 10.5, fontWeight: '600' },
  description: { fontSize: 14, lineHeight: 20 },
  scopeLine: { alignItems: 'center', flexDirection: 'row', gap: 5 },
  scopeText: { flexShrink: 1, fontSize: 11.5 },
  meta: {
    borderRadius: Radius.medium,
    borderWidth: StyleSheet.hairlineWidth,
    paddingHorizontal: 13,
    paddingVertical: 2,
  },
  metaRow: { borderBottomWidth: StyleSheet.hairlineWidth, gap: 3, paddingVertical: 9 },
  metaLabel: { fontSize: 11, fontWeight: '600', textTransform: 'uppercase' },
  metaValue: { fontSize: 13, lineHeight: 18 },
  notice: { alignItems: 'flex-start', flexDirection: 'row', gap: 6, paddingVertical: 9 },
  noticeText: { flex: 1, fontSize: 11.5, lineHeight: 16 },
  actions: { alignItems: 'center', flexDirection: 'row', gap: 10 },
  actionsSpacer: { flex: 1 },
  deleteButton: {
    alignItems: 'center',
    borderRadius: Radius.small,
    borderWidth: StyleSheet.hairlineWidth,
    flexDirection: 'row',
    gap: 6,
    minHeight: 34,
    paddingHorizontal: 12,
  },
  deleteText: { fontSize: 13, fontWeight: '500' },
  cancelButton: { justifyContent: 'center', minHeight: 34, paddingHorizontal: 4 },
  cancelText: { fontSize: 13 },
  installNote: { fontSize: 11.5 },
  contents: { gap: 8 },
  sectionTitle: {
    fontSize: 12,
    fontWeight: '600',
    letterSpacing: 0.4,
    textTransform: 'uppercase',
  },
});
